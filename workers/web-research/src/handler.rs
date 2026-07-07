//! stub — implemented in Task 2.4
pub struct WebResearchHandler;
impl WebResearchHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        anyhow::bail!("web-research handler not yet implemented")
    }
}

impl kastellan_protocol::server::Handler for WebResearchHandler {
    fn call(&mut self, _m: &str, _p: serde_json::Value)
        -> Result<serde_json::Value, kastellan_protocol::RpcError> {
        Err(kastellan_protocol::RpcError::new(
            kastellan_protocol::codes::METHOD_NOT_FOUND, "stub"))
    }
}
