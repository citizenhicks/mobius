//! Remote interface images use the same ring-backed reqwest as the framework.

use std::io;
use std::time::Duration;

use futures_util::TryStreamExt;
use futures_util::future::BoxFuture;
use gpui::http_client::{
    AsyncBody, HttpClient, Inner, Method, Request, RequestTimeout, Response, Url,
};
use hyper::header::HeaderValue;
use reqwest::Client;
use tokio::runtime::{Builder, Runtime};
use tokio_util::task::AbortOnDropHandle;

pub(crate) struct ImageClient {
    client: Client,
    runtime: Runtime,
    user_agent: HeaderValue,
}

impl ImageClient {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let user_agent = HeaderValue::from_static(concat!("mobius/", env!("CARGO_PKG_VERSION")));
        let client = Client::builder()
            .user_agent(&user_agent)
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(100))
            .build()?;
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        Ok(Self {
            client,
            runtime,
            user_agent,
        })
    }
}

impl HttpClient for ImageClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        use http_body_util::BodyExt as _;
        let (parts, body) = request.into_parts();
        // shortcut: GPUI image loading uses empty GETs with redirects; extend only for a new consumer.
        if parts.method != Method::GET || !matches!(body.0, Inner::Empty) {
            return Box::pin(async {
                anyhow::bail!("the image client only accepts empty GET requests")
            });
        }
        let mut request = self
            .client
            .get(parts.uri.to_string())
            .headers(parts.headers);
        if let Some(timeout) = parts.extensions.get::<RequestTimeout>() {
            request = request.timeout(timeout.0);
        }
        let task = AbortOnDropHandle::new(self.runtime.spawn(async move {
            let response: Response<reqwest::Body> = request
                .send()
                .await
                .map_err(reqwest::Error::without_url)?
                .into();
            Ok(response.map(|body| {
                AsyncBody::from_reader(
                    body.into_data_stream()
                        .map_err(io::Error::other)
                        .into_async_read(),
                )
            }))
        }));
        Box::pin(async move { task.await? })
    }
}

#[cfg(test)]
mod tests;
