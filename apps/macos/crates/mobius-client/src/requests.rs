//! Request/reply routing on one authenticated connection, backed by `mobius_gateway::client`.

pub(super) use mobius_gateway::client::{
    GatewayLink as Link, GatewayRequests as Requests, RequestUsage as Usage,
    request_disconnected as disconnected,
};
