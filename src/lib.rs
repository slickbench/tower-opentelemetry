#![feature(backtrace)]
#![warn(clippy::pedantic)]
use std::{borrow::Cow, error::Error as StdError, future::Future, pin::Pin, task::Poll, sync::Arc};

use futures_util::future::FutureExt;
use http::{
    header::{self, HeaderName},
    HeaderValue, Method, Request, Response, Version,
};
use lazy_static::lazy_static;
use opentelemetry::{
    global,
    propagation::{Extractor, Injector},
    trace::{FutureExt as OtelFutureExt, SpanKind, StatusCode, TraceContextExt, Tracer},
    Context,
};
use opentelemetry_semantic_conventions::trace::{
    HTTP_FLAVOR, HTTP_METHOD, HTTP_STATUS_CODE, HTTP_TARGET, HTTP_URL, HTTP_USER_AGENT,
    NET_HOST_NAME,
};
use sysinfo::{System, SystemExt};

lazy_static! {
    static ref SYSTEM: System = System::new_all();
}

#[inline]
fn http_method_str(method: &Method) -> Cow<'static, str> {
    match method {
        &Method::OPTIONS => "OPTIONS".into(),
        &Method::GET => "GET".into(),
        &Method::POST => "POST".into(),
        &Method::PUT => "PUT".into(),
        &Method::DELETE => "DELETE".into(),
        &Method::HEAD => "HEAD".into(),
        &Method::TRACE => "TRACE".into(),
        &Method::CONNECT => "CONNECT".into(),
        &Method::PATCH => "PATCH".into(),
        other => other.to_string().into(),
    }
}

#[inline]
fn http_flavor(version: Version) -> Cow<'static, str> {
    match version {
        Version::HTTP_09 => "0.9".into(),
        Version::HTTP_10 => "1.0".into(),
        Version::HTTP_11 => "1.1".into(),
        Version::HTTP_2 => "2.0".into(),
        Version::HTTP_3 => "3.0".into(),
        other => format!("{:?}", other).into(),
    }
}

/// [`Layer`] that adds high level [opentelemetry propagation] to a [`Service`].
///
/// [`Layer`]: tower_layer::Layer
/// [opentelemetry propagation]: https://opentelemetry.io/docs/java/manual_instrumentation/#context-propagation
/// [`Service`]: tower_service::Service
#[derive(Debug, Copy, Clone, Default)]
pub struct Layer {}

impl Layer {
    /// Create a new [`TraceLayer`] using the given [`MakeClassifier`].
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }
}

impl<S> tower_layer::Layer<S> for Layer where S: Clone {
    type Service = Service<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Service::new(inner)
    }
}

/// Middleware [`Service`] that propagates the opentelemetry trace header, configures a span for
/// the request, and records any exceptions.
///
/// [`Service`]: tower_service::Service
#[derive(Clone)]
pub struct Service<S: Clone> {
    inner: S,
    tracer: Arc<global::BoxedTracer>,
}

impl<S> Service<S> where S: Clone {
    fn new(inner: S) -> Self {
        Self {
            inner,
            tracer: Arc::new(global::tracer_with_version("tower-opentelemetry", env!("CARGO_PKG_VERSION"))),
        }
    }
}

type CF<R, E> = dyn Future<Output = Result<R, E>> + Send;
impl<B, ResBody, S> tower_service::Service<Request<B>> for Service<S>
where
    S: tower_service::Service<Request<B>, Response = Response<ResBody>>,
    S::Future: 'static + Send,
    B: 'static,
    S::Error: std::fmt::Debug + StdError,
    S: Clone
{
    type Error = S::Error;
    type Future = Pin<Box<CF<Self::Response, Self::Error>>>;
    type Response = S::Response;

    #[inline]
    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let parent_context = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&HeaderCarrier::new(req.headers_mut()))
        });
        // let conn_info = req.connection_info();
        let uri = req.uri();
        let mut builder = self
            .tracer
            .span_builder(uri.path().to_string())
            .with_parent_context(parent_context)
            .with_kind(SpanKind::Server);
        let mut attributes = Vec::with_capacity(11);
        attributes.push(HTTP_METHOD.string(http_method_str(req.method())));
        attributes.push(HTTP_FLAVOR.string(http_flavor(req.version())));
        attributes.push(HTTP_URL.string(uri.to_string()));

        if let Some(host_name) = SYSTEM.host_name() {
            attributes.push(NET_HOST_NAME.string(host_name));
        }

        if let Some(path) = uri.path_and_query() {
            attributes.push(HTTP_TARGET.string(path.as_str().to_string()));
        }
        if let Some(user_agent) = req
            .headers()
            .get(header::USER_AGENT)
            .and_then(|s| s.to_str().ok())
        {
            attributes.push(HTTP_USER_AGENT.string(user_agent.to_string()));
        }
        builder.attributes = Some(attributes);
        let span = self.tracer.build(builder);
        let cx = Context::current_with_span(span);
        let attachment = cx.clone().attach();

        let fut = self
            .inner
            .call(req)
            .with_context(cx.clone())
            .map(move |res| match res {
                Ok(mut ok_res) => {
                    opentelemetry::global::get_text_map_propagator(|propagator| {
                        propagator.inject(&mut HeaderCarrier::new(ok_res.headers_mut()));
                    });
                    let span = cx.span();
                    span.set_attribute(HTTP_STATUS_CODE.i64(i64::from(ok_res.status().as_u16())));
                    if ok_res.status().is_server_error() {
                        span.set_status(
                            StatusCode::Error,
                            ok_res
                                .status()
                                .canonical_reason()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        );
                    };
                    span.end();
                    Ok(ok_res)
                }
                Err(error) => {
                    let span = cx.span();
                    span.set_status(StatusCode::Error, format!("{:?}", error));
                    if let Some(backtrace) = error.backtrace() {
                        span.record_exception_with_stacktrace(&error, backtrace.to_string());
                    } else {
                        span.record_exception(&error);
                    }
                    span.end();
                    Err(error)
                }
            });

        drop(attachment);
        Box::pin(fut)
    }
}

struct HeaderCarrier<'a> {
    headers: &'a mut http::HeaderMap,
}

impl<'a> HeaderCarrier<'a> {
    fn new(headers: &'a mut http::HeaderMap) -> Self {
        HeaderCarrier { headers }
    }
}

impl<'a> Extractor for HeaderCarrier<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.keys().map(HeaderName::as_str).collect()
    }
}

impl<'a> Injector for HeaderCarrier<'a> {
    fn set(&mut self, key: &str, value: String) {
        self.headers.insert(
            HeaderName::from_bytes(key.as_bytes()).expect("invalid header name"),
            HeaderValue::from_str(&value).expect("invalid header value"),
        );
    }
}

#[cfg(test)]
mod tests {}
