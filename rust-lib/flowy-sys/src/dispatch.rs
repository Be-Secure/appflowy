use crate::{
    error::{Error, InternalError, SystemError},
    module::{as_module_map, Event, Module, ModuleMap, ModuleRequest},
    request::Payload,
    response::EventResponse,
    service::{Service, ServiceFactory},
    util::tokio_default_runtime,
};
use derivative::*;
use futures_core::future::BoxFuture;
use futures_util::task::Context;
use lazy_static::lazy_static;
use pin_project::pin_project;
use std::{
    fmt::{Debug, Display},
    future::Future,
    hash::Hash,
    sync::RwLock,
    thread::JoinHandle,
};
use tokio::{
    macros::support::{Pin, Poll},
    task::JoinError,
};

lazy_static! {
    pub static ref EVENT_DISPATCH: RwLock<Option<EventDispatch>> = RwLock::new(None);
}

pub struct EventDispatch {
    module_map: ModuleMap,
    runtime: tokio::runtime::Runtime,
}

impl EventDispatch {
    pub fn construct<F>(module_factory: F)
    where
        F: FnOnce() -> Vec<Module>,
    {
        let modules = module_factory();
        log::debug!("{}", module_info(&modules));
        let module_map = as_module_map(modules);
        let runtime = tokio_default_runtime().unwrap();
        let dispatch = EventDispatch {
            module_map,
            runtime,
        };

        *(EVENT_DISPATCH.write().unwrap()) = Some(dispatch);
    }

    pub fn async_send(request: DispatchRequest) -> DispatchFuture {
        match EVENT_DISPATCH.read() {
            Ok(dispatch) => {
                let dispatch = dispatch.as_ref().unwrap();
                let module_map = dispatch.module_map.clone();
                let service = Box::new(DispatchService { module_map });
                log::trace!("{}: dispatch {:?} to runtime", &request.id, &request.event);
                let join_handle = dispatch.runtime.spawn(async move {
                    service
                        .call(request)
                        .await
                        .unwrap_or_else(|e| InternalError::new(format!("{:?}", e)).as_response())
                });

                DispatchFuture {
                    fut: Box::pin(async move {
                        join_handle.await.unwrap_or_else(|e| {
                            InternalError::new(format!("Dispatch join error: {:?}", e))
                                .as_response()
                        })
                    }),
                }
            },

            Err(e) => {
                let msg = format!("Dispatch runtime error: {:?}", e);
                log::trace!("{}", msg);
                DispatchFuture {
                    fut: Box::pin(async { InternalError::new(msg).as_response() }),
                }
            },
        }
    }

    pub fn sync_send(request: DispatchRequest) -> EventResponse {
        futures::executor::block_on(async { EventDispatch::async_send(request).await })
    }
}

#[pin_project]
pub struct DispatchFuture {
    #[pin]
    fut: BoxFuture<'static, EventResponse>,
}

impl Future for DispatchFuture {
    type Output = EventResponse;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        loop {
            return Poll::Ready(futures_core::ready!(this.fut.poll(cx)));
        }
    }
}

pub type BoxFutureCallback =
    Box<dyn FnOnce(EventResponse) -> BoxFuture<'static, ()> + 'static + Send + Sync>;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct DispatchRequest {
    pub id: String,
    pub event: Event,
    pub payload: Payload,
    #[derivative(Debug = "ignore")]
    pub callback: Option<BoxFutureCallback>,
}

impl DispatchRequest {
    pub fn new<E>(event: E) -> Self
    where
        E: Eq + Hash + Debug + Clone + Display,
    {
        Self {
            payload: Payload::None,
            event: event.into(),
            id: uuid::Uuid::new_v4().to_string(),
            callback: None,
        }
    }

    pub fn payload(mut self, payload: Payload) -> Self {
        self.payload = payload;
        self
    }

    pub fn callback(mut self, callback: BoxFutureCallback) -> Self {
        self.callback = Some(callback);
        self
    }

    pub(crate) fn into_parts(self) -> (ModuleRequest, Option<BoxFutureCallback>) {
        let DispatchRequest {
            event,
            payload,
            id,
            callback,
        } = self;

        (ModuleRequest::new(event.clone(), id, payload), callback)
    }
}

pub(crate) struct DispatchService {
    pub(crate) module_map: ModuleMap,
}

impl Service<DispatchRequest> for DispatchService {
    type Response = EventResponse;
    type Error = SystemError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    #[cfg_attr(
        feature = "use_tracing",
        tracing::instrument(
            name = "DispatchService",
            level = "debug",
            skip(self, dispatch_request)
        )
    )]
    fn call(&self, dispatch_request: DispatchRequest) -> Self::Future {
        let module_map = self.module_map.clone();
        let (request, callback) = dispatch_request.into_parts();
        Box::pin(async move {
            let result = {
                match module_map.get(&request.event()) {
                    Some(module) => {
                        let fut = module.new_service(());
                        log::trace!(
                            "{}: handle event: {:?} by {}",
                            request.id(),
                            request.event(),
                            module.name
                        );
                        let service_fut = fut.await?.call(request);
                        service_fut.await
                    },
                    None => {
                        let msg = format!(
                            "Can not find the module to handle the request:{:?}",
                            request
                        );
                        log::trace!("{}", msg);
                        Err(InternalError::new(msg).into())
                    },
                }
            };

            let response = result.unwrap_or_else(|e| e.into());
            log::trace!("Dispatch result: {:?}", response);
            if let Some(callback) = callback {
                callback(response.clone()).await;
            }

            Ok(response)
        })
    }
}

fn module_info(modules: &Vec<Module>) -> String {
    let mut info = format!("{} modules loaded\n", modules.len());
    for module in modules {
        info.push_str(&format!("-> {} loaded \n", module.name));
    }
    info
}
