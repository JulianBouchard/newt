use std::future::Future;

use crate::message::Message;

#[derive(Debug, Clone, PartialEq)]
pub struct Steered {
    pub id: String,
    pub message: Message,
}

pub trait Steering: Send + Sync {
    fn take_new(&self) -> impl Future<Output = Vec<Steered>> + Send;
    fn arrived(&self) -> impl Future<Output = ()> + Send;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoSteering;

impl Steering for NoSteering {
    fn take_new(&self) -> impl Future<Output = Vec<Steered>> + Send {
        std::future::ready(Vec::new())
    }

    fn arrived(&self) -> impl Future<Output = ()> + Send {
        std::future::pending()
    }
}
