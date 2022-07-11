use std::{
    collections::HashMap,
    hash::Hash,
    path::{Path, PathBuf},
};

use async_stream::try_stream;
pub use futures::{pin_mut, Stream, StreamExt};
use inotify::{EventMask, Inotify, WatchMask};
use maplit::{btreeset, hashmap};
use strum_macros::Display;
use tracing::warn;
use try_traits::default::TryDefault;

#[derive(Debug, Display, PartialEq, Eq, Clone, Hash, PartialOrd, Ord)]
pub enum Masks {
    Modified,
    Deleted,
    Created,
    Undefined,
}

impl From<Masks> for WatchMask {
    fn from(masks: Masks) -> Self {
        match masks {
            Masks::Modified => WatchMask::MODIFY,
            Masks::Deleted => WatchMask::DELETE,
            Masks::Created => WatchMask::CREATE,
            Masks::Undefined => WatchMask::empty(),
        }
    }
}

impl From<EventMask> for Masks {
    fn from(em: EventMask) -> Self {
        match em {
            EventMask::MODIFY => Masks::Modified,
            EventMask::DELETE => Masks::Deleted,
            EventMask::CREATE => Masks::Created,
            _ => Masks::Undefined,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyStreamError {
    #[error(transparent)]
    FromIOError(#[from] std::io::Error),

    #[error("Error starting fs notification service.")]
    NotifyInitError,

    #[error("Error creating event stream")]
    ErrorCreatingStream,

    #[error("Error normalising watcher for: {path:?}")]
    ErrorNormalisingWatcher { path: PathBuf },

    #[error("Unsupported mask: {mask:?}")]
    UnsupportedWatchMask { mask: WatchMask },

    #[error("Expected watch directory to be: {expected:?} but was: {actual:?}")]
    WrongParentDirectory { expected: PathBuf, actual: PathBuf },

    #[error("Watcher: {mask} is duplicated for file: {path:?}")]
    DuplicateWatcher { mask: Masks, path: PathBuf },
}

#[derive(Debug, Default, Clone)]
struct WatchDescriptor {
    pub description: HashMap<PathBuf, HashMap<String, Vec<Masks>>>,
}

impl WatchDescriptor {
    fn new() -> Self {
        Self::default()
    }

    /// inserts new values in `self.description`. this takes care of inserting
    /// - new keys (dir_path, file_name)
    /// - inserting or appending new masks
    /// NOTE: though it is not a major concern, the `masks` entry is unordered
    /// vec![Masks::Deleted, Masks::Modified] does not equal vec![Masks::Modified, Masks::Deleted]
    fn insert(&mut self, dir_path: PathBuf, file_name: String, masks: Vec<Masks>) {
        let root_directory_entry = self.description.entry(dir_path).or_insert(hashmap! {});
        let file_entry = root_directory_entry
            .entry(file_name)
            .or_insert_with(|| masks.clone());
        // if `entry_masks` does not contain some of `masks`, insert them.
        for mask in &masks {
            if !file_entry.contains(mask) {
                file_entry.push(mask.to_owned())
            }
        }
    }

    /// get a set of `Masks` for a given `dir_path`
    fn get_mask_set_for_directory(&self, dir_path: PathBuf) -> Option<Vec<Masks>> {
        let mut set = btreeset! {};
        let hash_map = self.description.get(&dir_path)?;

        for masks in hash_map.values() {
            for mask in masks {
                set.insert(mask.to_owned());
            }
        }

        let v = Vec::from_iter(set);
        Some(v)
    }
}

//impl Eq for WatchDescriptor {}
//
//impl PartialEq for WatchDescriptor {
//    fn eq(&self, other: &Self) -> bool {
//        true
//        //self.dir_path == other.dir_path && other.key.keys().all(|f| self.key.keys().any(|x| x == f))
//        //self.dir_path == other.dir_path && self.key == other.key
//    }
//}

//impl Hash for WatchDescriptor {
//    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
//        todo!()
//        //self.dir_path.hash(state);
//        //for key in self.key.keys() {
//        //    key.hash(state);
//        //}
//    }
//}

pub struct NotifyStream {
    buffer: [u8; 1024],
    inotify: Inotify,
    watchers: WatchDescriptor,
}

impl TryDefault for NotifyStream {
    type Error = NotifyStreamError;

    fn try_default() -> Result<Self, Self::Error> {
        let inotify = Inotify::init();
        match inotify {
            Ok(inotify) => {
                let buffer = [0; 1024];
                Ok(Self {
                    buffer,
                    inotify,
                    watchers: WatchDescriptor::default(),
                })
            }
            Err(err) => Err(NotifyStreamError::FromIOError(err)),
        }
    }
}

/// normalisation step joining `candidate_watch_dir` and `candidate_file` and computing the parent of `candidate_file`.
///
/// this is useful in situations where:
/// `candidate_watch_dir` = /path/to/a/directory
/// `candidate_file` = continued/path/to/a/file
///
/// this function will concatenate the two, into:
/// `/path/to/a/directory/continued/path/to/a/file`
/// and will return:
/// `/path/to/a/directory/continued/path/to/a/` and `file`
fn normalising_watch_dir_and_file(
    candidate_watch_dir: &Path,
    candidate_file: &str,
) -> Result<(PathBuf, String), NotifyStreamError> {
    let full_path = candidate_watch_dir.join(candidate_file);
    let full_path = &full_path;
    let parent = full_path
        .parent()
        .ok_or_else(|| NotifyStreamError::ErrorNormalisingWatcher {
            path: full_path.to_path_buf(),
        })?;
    let file = full_path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| NotifyStreamError::ErrorNormalisingWatcher {
            path: full_path.to_path_buf(),
        })?;

    Ok((parent.to_path_buf(), file.to_string()))
}

/// to allow notify to watch for multiple events (CLOSE_WRITE, CREATE, MODIFY, etc...)
/// our internal enum `Masks` needs to be converted into a single `WatchMask` via bitwise OR
/// operations. (Note, our `Masks` type is an enum, `WatchMask` is a bitflag)
pub(crate) fn pipe_masks_into_watch_mask(masks: &[Masks]) -> WatchMask {
    let mut watch_mask = WatchMask::empty();
    for mask in masks {
        watch_mask |= mask.clone().into()
    }
    watch_mask
}

impl NotifyStream {
    /// add a watcher to a file or to a directory
    ///
    /// this is implemeted as a direcotry watcher regardless if a file is desired
    /// to be watched or if a directory. There is an internal data structure that
    /// keeps track of what is being watched - `self.watchers`
    /// The `stream` method determines whether the incoming event matches what is
    /// expected in `self.watchers`.
    ///
    /// # Watching directories
    ///
    /// ```rust
    /// use tedge_utils::fs_notify::{NotifyStream, Masks};
    /// use try_traits::default::TryDefault;
    /// use std::path::Path;
    ///
    /// let dir_path_a = Path::new("/tmp");
    /// let dir_path_b = Path::new("/etc/tedge/c8y");
    ///
    /// let mut fs_notification_stream = NotifyStream::try_default().unwrap();
    /// fs_notification_stream.add_watcher(dir_path_a, String::from("*"), &[Masks::Created]).unwrap();
    /// fs_notification_stream.add_watcher(dir_path_b, String::from("*"), &[Masks::Created, Masks::Deleted]).unwrap();
    /// ```
    ///
    /// # Watching files
    ///
    /// ```rust
    /// use tedge_utils::fs_notify::{NotifyStream, Masks};
    /// use tedge_test_utils::fs::TempTedgeDir;
    /// use try_traits::default::TryDefault;
    ///
    /// let ttd = TempTedgeDir::new();  // created a new tmp directory
    /// let file_a = ttd.file("file_a");
    /// let file_b = ttd.file("file_b");
    ///
    /// let mut fs_notification_stream = NotifyStream::try_default().unwrap();
    /// fs_notification_stream.add_watcher(ttd.path(), String::from("file_a"), &[Masks::Modified]).unwrap();
    /// fs_notification_stream.add_watcher(ttd.path(), String::from("file_b"), &[Masks::Created, Masks::Deleted]).unwrap();
    /// ```
    /// NOTE:
    /// in this last example, the root directory is the same: `ttd.path()`
    /// but the files watched and masks are different. In the background,
    /// the `add_watcher` fn will add a watch on `ttd.path()` with masks:
    /// Created, Modified and Deleted. and will update `self.watchers`
    /// with two entries, one for file_a and one for file_b.
    ///
    /// The `stream` method will check that events coming from
    /// `ttd.path()` match `self.watchers`
    pub fn add_watcher(
        &mut self,
        dir_path: &Path,
        file: String,
        masks: &[Masks],
    ) -> Result<(), NotifyStreamError> {
        let (dir_path, file) = normalising_watch_dir_and_file(dir_path, &file)?;
        let dir_path = dir_path.as_path();

        if self.watchers.description.is_empty() {
            let watch_mask = pipe_masks_into_watch_mask(masks);
            let _ = self.inotify.add_watch(dir_path, watch_mask);
            let mut wd = WatchDescriptor::new();
            wd.insert(dir_path.to_path_buf(), file, masks.to_vec());
            self.watchers = wd;
        } else {
            self.watchers
                .insert(dir_path.to_path_buf(), file, masks.to_vec());
            let masks = self
                .watchers
                .get_mask_set_for_directory(dir_path.to_path_buf()) // TODO: fix unwrap
                .unwrap();

            let watch_mask = pipe_masks_into_watch_mask(&masks);
            let _ = self.inotify.add_watch(dir_path, watch_mask);
        }
        Ok(())
    }

    //// create an fs notification event stream
    pub fn stream(mut self) -> impl Stream<Item = Result<(PathBuf, Masks), NotifyStreamError>> {
        try_stream! {
            let mut notify_service = self.inotify.event_stream(self.buffer)?;
            while let Some(event_or_error) = notify_service.next().await {
                match event_or_error {
                    Ok(event) => {
                        let event_mask: Masks = event.mask.into();
                        // in case the watch mask matches to an unsupported event, print this out
                        // as a warning so that it this event is transparent to the user of the
                        // crate.
                        if let Masks::Undefined = event_mask {
                            warn!("Unsupported mask: {:?}", event.mask);
                        }
                        // because watching a file or watching a direcotry is implemented as
                        // watching a directory, we can ignore the case where &event.name is None
                        if let Some(event_name) = &event.name {
                            let notify_file_name = event_name.to_str().ok_or_else(|| NotifyStreamError::ErrorCreatingStream)?;
                            // inotify triggered for a file named `notify_file_name`. Next we need
                            // to see if we have a matching entry WITH a matching flag/mask in `self.watchers`
                            for (dir_path, key) in &self.watchers.description {
                                for (file_name, flags) in key {
                                    for flag in flags {
                                        // There are two cases:
                                        // 1. we added a file watch
                                        // 2. we added a directory watch
                                        //
                                        // for case 1. our input could have been something like:
                                        // ...
                                        // notify_service.add_watcher(
                                        //          "/path/to/some/place",
                                        //          "file_name",    <------ note file name is given
                                        //          &[Masks::Created]
                                        //  )
                                        // here the file we are watching is *given* - so we can yield events with the
                                        // corresponding `event_name` and mask.
                                        if file_name.eq(notify_file_name) && event_mask.clone().eq(flag) {
                                            let full_path = dir_path.join(file_name.clone());
                                            yield (full_path, event_mask.clone())
                                        // for case 2. our input could have been something like:
                                        // notify_service.add_watcher(
                                        //          "/path/to/some/place",
                                        //          "*",            <------ note the file name is not given
                                        //          &[Masks::Created]
                                        //  )
                                        // here the file we are watching is not known to us, so we match only on event mask
                                        } else if file_name.eq("*")  && event_mask.clone().eq(flag) {
                                            let full_path = dir_path.join(notify_file_name);
                                            yield (full_path, event_mask.clone())
                                        }
                                    }
                                }

                            }
                        }
                        // there should never be an "if let None = &event.name" because add_watcher
                        // will always add a watcher as a directory
                    },
                    Err(error) => {
                        // any error comming out of `notify_service.next()` will be
                        // an std::Io error: https://docs.rs/inotify/latest/src/inotify/stream.rs.html#48
                        yield Err(NotifyStreamError::FromIOError(error))?;
                    }
                }
            }
        }
    }
}

/// utility function to return an fs notify stream:
///
/// this supports both file wathes and directory watches:
///
/// # Example
/// ```rust
/// use tedge_utils::fs_notify::{fs_notify_stream, Masks};
/// use tedge_test_utils::fs::TempTedgeDir;
///
/// // created a new tmp directory with some files and directories
/// let ttd = TempTedgeDir::new();
/// let file_a = ttd.file("file_a");
/// let file_b = ttd.file("file_b");
/// let file_c = ttd.dir("some_directory").file("file_c");
///
///
/// let fs_notification_stream = fs_notify_stream(&[
///      (ttd.path(), String::from("file_a"), &[Masks::Created]),
///      (ttd.path(), String::from("file_b"), &[Masks::Modified, Masks::Created]),
///      (ttd.path(), String::from("some_directory/file_c"), &[Masks::Deleted])
///     ]
/// ).unwrap();
/// ```
pub fn fs_notify_stream(
    input: &[(&Path, String, &[Masks])],
) -> Result<impl Stream<Item = Result<(PathBuf, Masks), NotifyStreamError>>, NotifyStreamError> {
    let mut fs_notification_service = NotifyStream::try_default()?;
    for (dir_path, file_name, flags) in input {
        fs_notification_service.add_watcher(dir_path, file_name.to_owned(), flags)?;
    }
    Ok(fs_notification_service.stream())
}

#[cfg(test)]
#[cfg(feature = "fs-notify")]
#[cfg(feature = "tracing")]
mod tests {
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use futures::{pin_mut, Stream, StreamExt};

    use maplit::hashmap;
    use tedge_test_utils::fs::TempTedgeDir;
    use try_traits::default::TryDefault;

    use crate::fs_notify::Masks;

    use super::{fs_notify_stream, NotifyStream, NotifyStreamError, WatchDescriptor};

    #[test]
    /// this test checks the underlying data structure `WatchDescriptor.description`
    /// three files are created:
    /// - file_a, file_b at root level of `TempTedgeDir`
    /// - file_c, at level: `TempTedgeDir`/new_dir
    fn test_watch_descriptor_data_field() {
        let ttd = TempTedgeDir::new();
        let new_dir = ttd.dir("new_dir");
        ttd.file("file_a");
        ttd.file("file_b");
        new_dir.file("file_c");

        let expected_data_structure = hashmap! {
            ttd.path().to_path_buf() => hashmap! {
                String::from("file_a") => vec![Masks::Created, Masks::Deleted],
                String::from("file_b") => vec![Masks::Created, Masks::Modified]
            },
            new_dir.path().to_path_buf() => hashmap! {
                String::from("file_c") => vec![Masks::Modified]
            }

        };
        let expected_hash_set_for_root_dir = vec![Masks::Modified, Masks::Deleted, Masks::Created];
        let expected_hash_set_for_new_dir = vec![Masks::Modified];

        let mut actual_data_structure = WatchDescriptor::new();
        actual_data_structure.insert(
            ttd.path().to_path_buf(),
            String::from("file_a"),
            vec![Masks::Created],
        );
        actual_data_structure.insert(
            ttd.path().to_path_buf(),
            String::from("file_b"),
            vec![Masks::Created, Masks::Modified],
        );
        actual_data_structure.insert(
            new_dir.path().to_path_buf(),
            String::from("file_c"),
            vec![Masks::Modified],
        );
        // NOTE: re-adding `file_a` with an extra mask
        actual_data_structure.insert(
            ttd.path().to_path_buf(),
            String::from("file_a"),
            vec![Masks::Deleted],
        );
        assert!(actual_data_structure
            .description
            .eq(&expected_data_structure));

        assert_eq!(
            actual_data_structure
                .get_mask_set_for_directory(ttd.path().to_path_buf())
                .unwrap(),
            expected_hash_set_for_root_dir
        );

        assert_eq!(
            actual_data_structure
                .get_mask_set_for_directory(new_dir.path().to_path_buf())
                .unwrap(),
            expected_hash_set_for_new_dir
        );
    }

    #[test]
    fn test_add_watcher() {
        let ttd = TempTedgeDir::new();
        let new_dir = ttd.dir("new_dir");
        ttd.file("file_a");
        ttd.file("file_b");
        new_dir.file("file_c");

        let mut notify_service = NotifyStream::try_default().unwrap();
        notify_service
            .add_watcher(ttd.path(), String::from("file_a"), &[Masks::Created])
            .unwrap();
        notify_service
            .add_watcher(
                ttd.path(),
                String::from("file_a"),
                &[Masks::Created, Masks::Deleted],
            )
            .unwrap();
        notify_service
            .add_watcher(ttd.path(), String::from("file_b"), &[Masks::Modified])
            .unwrap();
        notify_service
            .add_watcher(new_dir.path(), String::from("file_c"), &[Masks::Deleted])
            .unwrap();
    }

    async fn assert_stream(
        mut inputs: HashMap<String, Vec<Masks>>,
        stream: Result<
            impl Stream<Item = Result<(PathBuf, Masks), NotifyStreamError>>,
            NotifyStreamError,
        >,
    ) {
        let stream = stream.unwrap();
        pin_mut!(stream);
        while let Some(Ok((path, flag))) = stream.next().await {
            let file_name = String::from(path.file_name().unwrap().to_str().unwrap());
            let mut values = inputs.get_mut(&file_name).unwrap().to_vec();
            let index = values.iter().position(|x| *x == flag).unwrap();
            values.remove(index);

            if values.is_empty() {
                inputs.remove(&file_name);
            } else {
                inputs.insert(file_name, values);
            }

            if inputs.is_empty() {
                break;
            }
        }
    }

    #[tokio::test]
    async fn test_multiple_known_files_watched() {
        let ttd = Arc::new(TempTedgeDir::new());
        let ttd_clone = ttd.clone();

        let expected_events = hashmap! {
            String::from("file_a") => vec![Masks::Created],
            String::from("file_b") => vec![Masks::Created, Masks::Modified]
        };

        let stream = fs_notify_stream(&[
            (ttd.path(), String::from("file_a"), &[Masks::Created]),
            (
                ttd.path(),
                String::from("file_b"),
                &[Masks::Created, Masks::Modified],
            ),
        ]);

        let fs_notify_handler = tokio::task::spawn(async move {
            assert_stream(expected_events, stream).await;
        });

        let file_handler = tokio::task::spawn(async move {
            ttd_clone.file("file_a").with_raw_content("content");
            ttd_clone.file("file_b").with_raw_content("content");
        });

        let () = fs_notify_handler.await.unwrap();
        let () = file_handler.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_unknown_files_watched() {
        let ttd = Arc::new(TempTedgeDir::new());
        ttd.file("file_b"); // creating this file before the fs notify service
        let ttd_clone = ttd.clone();

        let expected_events = hashmap! {
            String::from("file_a") => vec![Masks::Created],
            String::from("file_b") => vec![Masks::Modified],
            String::from("file_c") => vec![Masks::Created, Masks::Deleted]
        };

        let stream = fs_notify_stream(&[(
            ttd.path(),
            String::from("*"),
            &[Masks::Created, Masks::Modified, Masks::Deleted],
        )]);

        let fs_notify_handler = tokio::task::spawn(async move {
            assert_stream(expected_events, stream).await;
        });

        let file_handler = tokio::task::spawn(async move {
            ttd_clone.file("file_a"); // should match CREATE
            ttd_clone.file("file_b").with_raw_content("content"); // should match MODIFY
            ttd_clone.file("file_c").delete(); // should match CREATE, DELETE
        });

        let () = fs_notify_handler.await.unwrap();
        let () = file_handler.await.unwrap();
    }
}