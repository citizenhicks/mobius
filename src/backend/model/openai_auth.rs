//! Authorization contract shared by OpenAI-compatible transports.

use std::borrow::Cow;
use std::sync::Arc;

use crate::BoxFuture;
use crate::Result;

pub(super) struct ResolvedAuthorization<'a> {
    pub token: Cow<'a, str>,
    pub headers: Vec<(&'static str, Cow<'a, str>)>,
}

pub(super) trait OpenAiAuthorization: Send + Sync {
    fn authorize_http<'a>(
        &'a self,
        streaming: bool,
        session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>>;

    fn authorize_websocket<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>>;

    fn recover_unauthorized<'a>(&'a self, rejected_token: &'a str) -> BoxFuture<'a, Result<bool>>;
}

pub(super) struct ApiKeyAuthorization(Arc<str>);

impl ApiKeyAuthorization {
    pub fn new(api_key: String) -> Self {
        Self(api_key.into())
    }
}

impl OpenAiAuthorization for ApiKeyAuthorization {
    fn authorize_http<'a>(
        &'a self,
        _streaming: bool,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        self.resolved()
    }

    fn authorize_websocket<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedAuthorization<'a>>> {
        self.resolved()
    }

    fn recover_unauthorized<'a>(&'a self, _rejected_token: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
}

impl ApiKeyAuthorization {
    fn resolved(&self) -> BoxFuture<'_, Result<ResolvedAuthorization<'_>>> {
        Box::pin(async move {
            Ok(ResolvedAuthorization {
                token: Cow::Borrowed(&self.0),
                headers: Vec::new(),
            })
        })
    }
}
