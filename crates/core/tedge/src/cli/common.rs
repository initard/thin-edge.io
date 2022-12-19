use std::fmt;

use tedge_config::system_services::SystemService;

#[derive(Copy, Clone, Debug)]
pub enum Cloud {
    C8y,
    Azure,
}

impl Cloud {
    pub fn dependent_mapper_service(&self) -> SystemService {
        match self {
            Cloud::Azure => SystemService::TEdgeMapperAz,
            Cloud::C8y => SystemService::TEdgeMapperC8y,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Azure => "Azure",
            Self::C8y => "Cumulocity",
        }
    }
}

impl fmt::Display for Cloud {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Cloud::C8y => write!(f, "Cumulocity"),
            Cloud::Azure => write!(f, "Azure"),
        }
    }
}
