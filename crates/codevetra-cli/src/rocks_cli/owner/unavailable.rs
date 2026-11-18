//! Explicit unavailable transport for platforms without Unix owner IPC.

use super::{Arc, Path, ToolCall, ToolCallContext, ToolError, ToolResult};

pub(super) struct OwnerClient;

impl OwnerClient {
    pub(super) fn connect(root: &Path) -> impl Future<Output = Result<Option<Self>, String>> {
        let _ = root;
        std::future::ready(Ok(None))
    }

    pub(super) fn call_owned(
        self,
        call: ToolCall,
        context: ToolCallContext,
        expected_owner: Option<String>,
    ) -> impl Future<Output = Result<(String, ToolResult), ToolError>> {
        let _ = (self, call, context, expected_owner);
        std::future::ready(Err(super::transport_error(
            "native project owner transport is unavailable",
        )))
    }
}

pub(super) fn watch(
    root: &Path,
    handler: Arc<crate::mcp_handler::RocksHandler>,
    shutdown: super::Shutdown,
) -> impl Future<Output = Result<(), String>> {
    let _ = (root, handler, shutdown);
    std::future::ready(Err("headless owner transport requires Unix".to_owned()))
}
