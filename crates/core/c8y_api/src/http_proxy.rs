use crate::json_c8y::{
    C8yCreateEvent, C8yEventResponse, C8yManagedObject, C8yUpdateSoftwareListResponse,
    InternalIdResponse,
};

use async_trait::async_trait;
use c8y_smartrest::{error::SMCumulocityMapperError, smartrest_deserializer::SmartRestJwtResponse};
use mockall::automock;
use mqtt_channel::{Connection, PubChannel, StreamExt, Topic, TopicFilter};
use reqwest::Url;
use std::{collections::HashMap, path::Path, time::Duration};
use tedge_config::{
    C8yUrlSetting, ConfigSettingAccessor, ConfigSettingAccessorStringExt, DeviceIdSetting,
    MqttBindAddressSetting, MqttPortSetting, TEdgeConfig,
};
use time::OffsetDateTime;

use tracing::{info, instrument};

/// An HttpProxy handles http requests to C8y on behalf of the device.
#[automock]
#[async_trait]
pub trait C8YHttpProxy: Send + Sync {
    fn url_is_in_my_tenant_domain(&mut self, url: &str) -> bool;

    async fn get_jwt_token(&mut self) -> Result<SmartRestJwtResponse, SMCumulocityMapperError>;

    async fn send_event(
        &mut self,
        c8y_event: C8yCreateEvent,
    ) -> Result<String, SMCumulocityMapperError>;

    async fn send_software_list_http(
        &mut self,
        c8y_software_list: &C8yUpdateSoftwareListResponse,
    ) -> Result<(), SMCumulocityMapperError>;

    async fn upload_log_binary(
        &mut self,
        log_content: &str,
    ) -> Result<String, SMCumulocityMapperError>;

    async fn upload_config_file(
        &mut self,
        config_path: &Path,
        config_content: &str,
    ) -> Result<String, SMCumulocityMapperError>;
}

/// Define a C8y endpoint
#[derive(Debug, Clone)]
pub struct C8yEndPoint {
    c8y_host: String,
    #[allow(dead_code)]
    device_id: String,
    c8y_internal_id: String,
}

#[derive(Debug, Clone)]
pub struct C8yEndPointUnInitialised {
    c8y_host: String,
    device_id: String,
}

impl C8yEndPointUnInitialised {
    fn get_base_url(&self) -> String {
        let mut url_get_id = String::new();
        if !self.c8y_host.starts_with("http") {
            url_get_id.push_str("https://");
        }
        url_get_id.push_str(&self.c8y_host);

        url_get_id
    }
    fn get_url_for_get_id(&self) -> String {
        let mut url_get_id = self.get_base_url();
        url_get_id.push_str("/identity/externalIds/c8y_Serial/");
        url_get_id.push_str(&self.device_id);

        url_get_id
    }
}

impl C8yEndPoint {
    fn new(uninitialised: &C8yEndPointUnInitialised, c8y_internal_id: String) -> C8yEndPoint {
        C8yEndPoint {
            c8y_host: uninitialised.c8y_host.clone(),
            device_id: uninitialised.device_id.clone(),
            c8y_internal_id,
        }
    }
    fn get_base_url(&self) -> String {
        let mut url_get_id = String::new();
        if !self.c8y_host.starts_with("http") {
            url_get_id.push_str("https://");
        }
        url_get_id.push_str(&self.c8y_host);

        url_get_id
    }

    fn get_url_for_sw_list(&self) -> String {
        let mut url_update_swlist = self.get_base_url();
        url_update_swlist.push_str("/inventory/managedObjects/");
        url_update_swlist.push_str(&self.c8y_internal_id.clone());

        url_update_swlist
    }

    #[allow(dead_code)]
    fn get_url_for_get_id(&self) -> String {
        let mut url_get_id = self.get_base_url();
        url_get_id.push_str("/identity/externalIds/c8y_Serial/");
        url_get_id.push_str(&self.device_id);

        url_get_id
    }

    fn get_url_for_create_event(&self) -> String {
        let mut url_create_event = self.get_base_url();
        url_create_event.push_str("/event/events/");

        url_create_event
    }

    fn get_url_for_event_binary_upload(&self, event_id: &str) -> String {
        let mut url_event_binary = self.get_url_for_create_event();
        url_event_binary.push_str(event_id);
        url_event_binary.push_str("/binaries");

        url_event_binary
    }

    fn url_is_in_my_tenant_domain(&self, url: &str) -> bool {
        // c8y URL may contain either `Tenant Name` or Tenant Id` so they can be one of following options:
        // * <tenant_name>.<domain> eg: sample.c8y.io
        // * <tenant_id>.<domain> eg: t12345.c8y.io
        // These URLs may be both equivalent and point to the same tenant.
        // We are going to remove that and only check if the domain is the same.
        let tenant_uri = &self.c8y_host;
        let url_host = match Url::parse(url) {
            Ok(url) => match url.host() {
                Some(host) => host.to_string(),
                None => return false,
            },
            Err(_err) => {
                return false;
            }
        };

        let url_domain = url_host.splitn(2, '.').collect::<Vec<&str>>();
        let tenant_domain = tenant_uri.splitn(2, '.').collect::<Vec<&str>>();
        if url_domain.get(1) == tenant_domain.get(1) {
            return true;
        }
        false
    }
}

#[automock]
#[async_trait]
pub trait C8yJwtTokenRetriever: Send + Sync {
    async fn get_jwt_token(&mut self) -> Result<SmartRestJwtResponse, SMCumulocityMapperError>;
}

pub struct C8yMqttJwtTokenRetriever {
    mqtt_con: mqtt_channel::Connection,
}

impl C8yMqttJwtTokenRetriever {
    pub fn new(mqtt_con: mqtt_channel::Connection) -> Self {
        C8yMqttJwtTokenRetriever { mqtt_con }
    }
}

#[async_trait]
impl C8yJwtTokenRetriever for C8yMqttJwtTokenRetriever {
    async fn get_jwt_token(&mut self) -> Result<SmartRestJwtResponse, SMCumulocityMapperError> {
        let () = self
            .mqtt_con
            .published
            .publish(mqtt_channel::Message::new(
                &Topic::new_unchecked("c8y/s/uat"),
                "".to_string(),
            ))
            .await?;
        let token_smartrest = match tokio::time::timeout(
            Duration::from_secs(10),
            self.mqtt_con.received.next(),
        )
        .await
        {
            Ok(Some(msg)) => msg.payload_str()?.to_string(),
            Ok(None) => return Err(SMCumulocityMapperError::InvalidMqttMessage),
            Err(_elapsed) => return Err(SMCumulocityMapperError::RequestTimeout),
        };

        Ok(SmartRestJwtResponse::try_new(&token_smartrest)?)
    }
}

/// An HttpProxy that uses MQTT to retrieve JWT tokens and authenticate the device
///
/// - Keep the connection info to c8y and the internal Id of the device
/// - Handle JWT requests
pub struct JwtAuthHttpProxy {
    jwt_token_retriver: Box<dyn C8yJwtTokenRetriever>,
    http_con: reqwest::Client,
    end_point: either::Either<C8yEndPointUnInitialised, C8yEndPoint>,
}

impl JwtAuthHttpProxy {
    pub fn new(
        jwt_token_retriver: Box<dyn C8yJwtTokenRetriever>,
        http_con: reqwest::Client,
        c8y_host: &str,
        device_id: &str,
    ) -> JwtAuthHttpProxy {
        JwtAuthHttpProxy {
            jwt_token_retriver,
            http_con,
            end_point: either::Either::Left(C8yEndPointUnInitialised {
                c8y_host: c8y_host.into(),
                device_id: device_id.into(),
            }),
        }
    }

    pub async fn try_new(
        tedge_config: &TEdgeConfig,
    ) -> Result<JwtAuthHttpProxy, SMCumulocityMapperError> {
        let c8y_host = tedge_config.query_string(C8yUrlSetting)?;
        let device_id = tedge_config.query_string(DeviceIdSetting)?;
        let http_con = reqwest::ClientBuilder::new().build()?;

        let mqtt_port = tedge_config.query(MqttPortSetting)?.into();
        let mqtt_host = tedge_config.query(MqttBindAddressSetting)?.to_string();
        let topic = TopicFilter::new("c8y/s/dat")?;
        let mqtt_config = mqtt_channel::Config::default()
            .with_port(mqtt_port)
            .with_clean_session(true)
            .with_host(mqtt_host)
            .with_subscriptions(topic);

        let mut mqtt_con = Connection::new(&mqtt_config).await?;

        // Ignore errors on this connection
        let () = mqtt_con.errors.close();

        let jwt_token_retriver = Box::new(C8yMqttJwtTokenRetriever::new(mqtt_con));

        Ok(JwtAuthHttpProxy::new(
            jwt_token_retriver,
            http_con,
            &c8y_host,
            &device_id,
        ))
    }

    async fn try_get_internal_id(&mut self) -> Result<String, SMCumulocityMapperError> {
        let token = self.get_jwt_token().await?;
        let url_get_id = self.end_point.as_ref().unwrap_left().get_url_for_get_id();

        let internal_id = self
            .http_con
            .get(url_get_id)
            .bearer_auth(token.token())
            .send()
            .await?;
        let internal_id_response = internal_id.json::<InternalIdResponse>().await?;

        let internal_id = internal_id_response.id();
        Ok(internal_id)
    }

    async fn create_log_event(&mut self) -> Result<C8yCreateEvent, SMCumulocityMapperError> {
        let c8y_end_point = self.get_c8y_end_point().await?;
        let c8y_managed_object = C8yManagedObject {
            id: c8y_end_point.c8y_internal_id,
        };

        Ok(C8yCreateEvent::new(
            Some(c8y_managed_object),
            "c8y_Logfile".to_string(),
            OffsetDateTime::now_utc(),
            "software-management".to_string(),
            HashMap::new(),
        ))
    }

    async fn create_event(
        &self,
        end_point: &C8yEndPoint,
        event_type: String,
        event_text: Option<String>,
        event_time: Option<OffsetDateTime>,
    ) -> Result<C8yCreateEvent, SMCumulocityMapperError> {
        let c8y_managed_object = C8yManagedObject {
            id: end_point.c8y_internal_id.clone(),
        };

        Ok(C8yCreateEvent::new(
            Some(c8y_managed_object),
            event_type.clone(),
            event_time.unwrap_or_else(OffsetDateTime::now_utc),
            event_text.unwrap_or(event_type),
            HashMap::new(),
        ))
    }

    async fn send_event_internal(
        &mut self,
        c8y_end_point: &C8yEndPoint,
        c8y_event: C8yCreateEvent,
    ) -> Result<String, SMCumulocityMapperError> {
        let token = self.get_jwt_token().await?;
        let create_event_url = c8y_end_point.get_url_for_create_event();

        let request = self
            .http_con
            .post(create_event_url)
            .json(&c8y_event)
            .bearer_auth(token.token())
            .header("Accept", "application/json")
            .timeout(Duration::from_millis(10000))
            .build()?;

        let response = self.http_con.execute(request).await?;
        let _ = response.error_for_status_ref()?;
        let event_response_body = response.json::<C8yEventResponse>().await?;

        Ok(event_response_body.id)
    }

    async fn get_c8y_end_point(&mut self) -> Result<C8yEndPoint, SMCumulocityMapperError> {
        match &self.end_point {
            either::Right(c8y_end_point) => {
                return Ok(c8y_end_point.clone());
            }
            either::Left(ref _uninitialised) => {}
        }
        self.init().await
    }

    #[instrument(skip(self), name = "init")]
    async fn init(&mut self) -> Result<C8yEndPoint, SMCumulocityMapperError> {
        if self.end_point.is_left() {
            info!("Initialisation");

            let uninitialised = self.end_point.as_ref().unwrap_left().clone();
            let c8y_end_point = match self.try_get_internal_id().await {
                Ok(internal_id) => Ok(C8yEndPoint::new(&uninitialised, internal_id)),
                Err(_error) => Err(SMCumulocityMapperError::FailedToRetrieveC8yInternalId),
            }?;
            self.end_point = either::Either::Right(c8y_end_point);
            info!("Initialisation done.");
        }
        Ok(self.end_point.clone().unwrap_right())
    }
}

#[async_trait]
impl C8YHttpProxy for JwtAuthHttpProxy {
    fn url_is_in_my_tenant_domain(&mut self, url: &str) -> bool {
        self.end_point
            .as_ref()
            .unwrap_right()
            .url_is_in_my_tenant_domain(url)
    }

    async fn get_jwt_token(&mut self) -> Result<SmartRestJwtResponse, SMCumulocityMapperError> {
        self.jwt_token_retriver.get_jwt_token().await
    }

    async fn send_event(
        &mut self,
        mut c8y_event: C8yCreateEvent,
    ) -> Result<String, SMCumulocityMapperError> {
        let c8y_end_point = { self.get_c8y_end_point().await?.clone() };
        if c8y_event.source.is_none() {
            c8y_event.source = Some(C8yManagedObject {
                id: c8y_end_point.c8y_internal_id.clone(),
            });
        }
        self.send_event_internal(&c8y_end_point, c8y_event).await
    }

    async fn send_software_list_http(
        &mut self,
        c8y_software_list: &C8yUpdateSoftwareListResponse,
    ) -> Result<(), SMCumulocityMapperError> {
        let c8y_end_point = self.get_c8y_end_point().await?;
        let url = c8y_end_point.get_url_for_sw_list();
        let token = self.get_jwt_token().await?;

        let request = self
            .http_con
            .put(url)
            .json(c8y_software_list)
            .bearer_auth(&token.token())
            .timeout(Duration::from_millis(10000))
            .build()?;

        let _response = self.http_con.execute(request).await?;

        Ok(())
    }

    async fn upload_log_binary(
        &mut self,
        log_content: &str,
    ) -> Result<String, SMCumulocityMapperError> {
        let token = self.get_jwt_token().await?;
        let c8y_end_point = self.get_c8y_end_point().await?;

        let log_event = self.create_log_event().await?;
        let event_response_id = self.send_event_internal(&c8y_end_point, log_event).await?;
        let binary_upload_event_url =
            c8y_end_point.get_url_for_event_binary_upload(&event_response_id);

        let request = self
            .http_con
            .post(&binary_upload_event_url)
            .header("Accept", "application/json")
            .header("Content-Type", "text/plain")
            .body(log_content.to_string())
            .bearer_auth(token.token())
            .timeout(Duration::from_millis(10000))
            .build()?;

        let _response = self.http_con.execute(request).await?;
        Ok(binary_upload_event_url)
    }

    async fn upload_config_file(
        &mut self,
        config_path: &Path,
        config_content: &str,
    ) -> Result<String, SMCumulocityMapperError> {
        let token = self.get_jwt_token().await?;

        let end_point = self.get_c8y_end_point().await?;
        let config_file_event = self
            .create_event(&end_point, config_path.display().to_string(), None, None)
            .await?;

        let event_response_id = self
            .send_event_internal(&end_point, config_file_event)
            .await?;
        let binary_upload_event_url = end_point.get_url_for_event_binary_upload(&event_response_id);

        let request = self
            .http_con
            .post(&binary_upload_event_url)
            .header("Accept", "application/json")
            .header("Content-Type", "text/plain")
            .body(config_content.to_string())
            .bearer_auth(token.token())
            .timeout(Duration::from_millis(10000))
            .build()?;

        let _response = self.http_con.execute(request).await?;
        Ok(binary_upload_event_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use mockito::{mock, Matcher};
    use serde_json::json;
    use test_case::test_case;

    #[test]
    fn get_url_for_get_id_returns_correct_address() {
        let c8y_uninitialised = C8yEndPointUnInitialised {
            c8y_host: "test_host".to_string(),
            device_id: "test_device".to_string(),
        };
        let c8y = C8yEndPoint::new(&c8y_uninitialised, "internal-id".to_string());
        let res = c8y.get_url_for_get_id();

        assert_eq!(
            res,
            "https://test_host/identity/externalIds/c8y_Serial/test_device"
        );
    }

    #[test]
    fn get_url_for_sw_list_returns_correct_address() {
        let c8y_uninitialised = C8yEndPointUnInitialised {
            c8y_host: "test_host".to_string(),
            device_id: "test_device".to_string(),
        };
        let c8y = C8yEndPoint::new(&c8y_uninitialised, "12345".to_string());
        let res = c8y.get_url_for_sw_list();

        assert_eq!(res, "https://test_host/inventory/managedObjects/12345");
    }

    #[test_case("http://aaa.test.com")]
    #[test_case("https://aaa.test.com")]
    #[test_case("ftp://aaa.test.com")]
    #[test_case("mqtt://aaa.test.com")]
    #[test_case("https://t1124124.test.com")]
    #[test_case("https://t1124124.test.com:12345")]
    #[test_case("https://t1124124.test.com/path")]
    #[test_case("https://t1124124.test.com/path/to/file.test")]
    #[test_case("https://t1124124.test.com/path/to/file")]
    fn url_is_my_tenant_correct_urls(url: &str) {
        let c8y_uninitialised = C8yEndPointUnInitialised {
            c8y_host: "test.test.com".to_string(),
            device_id: "test_device".to_string(),
        };
        let c8y = C8yEndPoint::new(&c8y_uninitialised, "internal-id".to_string());
        assert!(c8y.url_is_in_my_tenant_domain(url));
    }

    #[test_case("test.com")]
    #[test_case("http://test.co")]
    #[test_case("http://test.co.te")]
    #[test_case("http://test.com:123456")]
    #[test_case("http://test.com::12345")]
    fn url_is_my_tenant_incorrect_urls(url: &str) {
        let c8y_uninitialised = C8yEndPointUnInitialised {
            c8y_host: "test.test.com".to_string(),
            device_id: "test_device".to_string(),
        };
        let c8y = C8yEndPoint::new(&c8y_uninitialised, "internal-id".to_string());
        assert!(!c8y.url_is_in_my_tenant_domain(url));
    }

    #[tokio::test]
    async fn get_internal_id() -> Result<()> {
        let device_id = "test-device";
        let internal_device_id = "1234";

        let _mock = mock("GET", "/identity/externalIds/c8y_Serial/test-device")
            .with_status(200)
            .with_body(
                json!({ "externalId": device_id, "managedObject": { "id": internal_device_id } })
                    .to_string(),
            )
            .create();

        // An JwtAuthHttpProxy ...
        let mut jwt_token_retriver = Box::new(MockC8yJwtTokenRetriever::new());
        jwt_token_retriver
            .expect_get_jwt_token()
            .returning(|| Ok(SmartRestJwtResponse::default()));

        let http_client = reqwest::ClientBuilder::new().build().unwrap();
        let mut http_proxy = JwtAuthHttpProxy::new(
            jwt_token_retriver,
            http_client,
            mockito::server_url().as_str(),
            device_id,
        );

        assert_eq!(http_proxy.try_get_internal_id().await?, internal_device_id);

        Ok(())
    }

    #[tokio::test]
    async fn send_event() -> anyhow::Result<()> {
        let device_id = "test-device";
        let event_id = "456";

        // Mock endpoint to return C8Y internal id
        let _get_internal_id_mock = mock("GET", "/identity/externalIds/c8y_Serial/test-device")
            .with_status(200)
            .with_body(
                json!({ "externalId": device_id, "managedObject": { "id": "123" } }).to_string(),
            )
            .create();

        let _create_event_mock = mock("POST", "/event/events/")
            .match_body(Matcher::PartialJson(
                json!({ "type": "clock_event", "text": "tick" }),
            ))
            .with_status(201)
            .with_body(json!({ "id": event_id }).to_string())
            .create();

        // An JwtAuthHttpProxy ...
        let mut jwt_token_retriver = Box::new(MockC8yJwtTokenRetriever::new());
        jwt_token_retriver
            .expect_get_jwt_token()
            .returning(|| Ok(SmartRestJwtResponse::default()));

        let http_client = reqwest::ClientBuilder::new().build().unwrap();
        let mut http_proxy = JwtAuthHttpProxy::new(
            jwt_token_retriver,
            http_client,
            mockito::server_url().as_str(),
            device_id,
        );

        let c8y_event = C8yCreateEvent::new(
            None,
            "clock_event".to_string(),
            OffsetDateTime::now_utc(),
            "tick".to_string(),
            HashMap::new(),
        );
        // ... creates the event and assert its id
        assert_eq!(http_proxy.send_event(c8y_event).await?, event_id);

        Ok(())
    }
}
