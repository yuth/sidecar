use crate::{
    agentic::tool::{base::Tool, errors::ToolError, input::ToolInput, output::ToolOutput},
    chunking::text_document::{Position, Range},
};
use async_trait::async_trait;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoToDefinitionRequest {
    fs_file_path: String,
    editor_url: String,
    position: Position,
}

impl GoToDefinitionRequest {
    pub fn new(fs_file_path: String, editor_url: String, position: Position) -> Self {
        Self {
            fs_file_path,
            editor_url,
            position,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoToDefinitionResponse {
    definitions: Vec<DefinitionPathAndRange>,
}

impl GoToDefinitionResponse {
    pub fn definitions(self) -> Vec<DefinitionPathAndRange> {
        self.definitions
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DefinitionPathAndRange {
    fs_file_path: String,
    range: Range,
}

impl DefinitionPathAndRange {
    pub fn file_path(&self) -> &str {
        &self.fs_file_path
    }

    pub fn range(&self) -> &Range {
        &self.range
    }
}

pub struct LSPGoToDefinition {
    client: reqwest::Client,
}

impl LSPGoToDefinition {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for LSPGoToDefinition {
    async fn invoke(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let context = input.is_go_to_definition()?;
        let editor_endpoint = context.editor_url.to_owned() + "/go_to_definition";
        let response = self
            .client
            .post(editor_endpoint)
            .body(serde_json::to_string(&context).map_err(|_e| ToolError::SerdeConversionFailed)?)
            .send()
            .await
            .map_err(|_e| ToolError::ErrorCommunicatingWithEditor)?;
        let response: GoToDefinitionResponse = response
            .json()
            .await
            .map_err(|_e| ToolError::SerdeConversionFailed)?;

        Ok(ToolOutput::GoToDefinition(response))
    }
}
