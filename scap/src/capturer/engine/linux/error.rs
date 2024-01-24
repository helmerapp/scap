use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use pipewire::spa::pod::serialize::GenError;

#[derive(Debug)]
pub struct LinCapError {
    msg: String,
}

impl Error for LinCapError {}

impl LinCapError {
    pub fn new(msg: String) -> Self {
        Self { msg }
    }
}

impl Display for LinCapError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl From<pipewire::Error> for LinCapError {
    fn from(e: pipewire::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<std::sync::mpsc::SendError<bool>> for LinCapError {
    fn from(e: std::sync::mpsc::SendError<bool>) -> Self {
        Self::new(e.to_string())
    }
}

impl From<ashpd::Error> for LinCapError {
    fn from(e: ashpd::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<GenError> for LinCapError {
    fn from(e: GenError) -> Self {
        Self::new(e.to_string())
    }
}