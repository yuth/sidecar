//! Here we grab the important symbols which we are going to edit or follow further
//! and figure out what we should be doing next
//! At each step we are going to focus on the current symbol and keep adding the
//! rest ones to our history and keep them, this is how agents are going to look like
//! These are like state-machines which are holding memory and moving forward and collaborating.

use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

use llm_client::{
    broker::LLMBroker,
    clients::types::LLMType,
    provider::{LLMProvider, LLMProviderAPIKeys},
};

use crate::{
    agentic::tool::{base::Tool, errors::ToolError, input::ToolInput, output::ToolOutput},
    chunking::text_document::Range,
    user_context::types::UserContext,
};

use super::{models::anthropic::AnthropicCodeSymbolImportant, types::CodeSymbolError};

pub struct CodeSymbolImportantBroker {
    llms: HashMap<LLMType, Box<dyn CodeSymbolImportant + Send + Sync>>,
}

impl CodeSymbolImportantBroker {
    pub fn new(llm_client: Arc<LLMBroker>) -> Self {
        let mut llms: HashMap<LLMType, Box<dyn CodeSymbolImportant + Send + Sync>> = HashMap::new();
        llms.insert(
            LLMType::ClaudeHaiku,
            Box::new(AnthropicCodeSymbolImportant::new(llm_client.clone())),
        );
        llms.insert(
            LLMType::ClaudeSonnet,
            Box::new(AnthropicCodeSymbolImportant::new(llm_client.clone())),
        );
        llms.insert(
            LLMType::ClaudeOpus,
            Box::new(AnthropicCodeSymbolImportant::new(llm_client.clone())),
        );
        llms.insert(
            LLMType::Gpt4O,
            Box::new(AnthropicCodeSymbolImportant::new(llm_client.clone())),
        );
        llms.insert(
            LLMType::GeminiPro,
            Box::new(AnthropicCodeSymbolImportant::new(llm_client.clone())),
        );
        Self { llms }
    }
}

#[async_trait]
impl Tool for CodeSymbolImportantBroker {
    async fn invoke(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        // PS: This is getting out of hand
        if input.is_utility_code_search() {
            let context = input.utility_code_search()?;
            if let Some(implementation) = self.llms.get(&context.model()) {
                return implementation
                    .gather_utility_symbols(context)
                    .await
                    .map(|response| ToolOutput::utility_code_symbols(response))
                    .map_err(|e| ToolError::CodeSymbolError(e));
            }
        } else {
            let context = input.code_symbol_search();
            if let Ok(context) = context {
                match context {
                    either::Left(context) => {
                        if let Some(implementation) = self.llms.get(context.model()) {
                            return implementation
                                .get_important_symbols(context)
                                .await
                                .map(|response| ToolOutput::important_symbols(response))
                                .map_err(|e| ToolError::CodeSymbolError(e));
                        }
                    }
                    either::Right(context) => {
                        if let Some(implementation) = self.llms.get(context.model()) {
                            return implementation
                                .context_wide_search(context)
                                .await
                                .map(|response| ToolOutput::important_symbols(response))
                                .map_err(|e| ToolError::CodeSymbolError(e));
                        }
                    }
                };
            }
        }
        Err(ToolError::WrongToolInput)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolImportantWideSearch {
    user_context: UserContext,
    user_query: String,
    llm_type: LLMType,
    llm_provider: LLMProvider,
    api_key: LLMProviderAPIKeys,
}

impl CodeSymbolImportantWideSearch {
    pub fn new(
        user_context: UserContext,
        user_query: String,
        llm_type: LLMType,
        llm_provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
    ) -> Self {
        Self {
            user_context,
            user_query,
            llm_type,
            llm_provider,
            api_key,
        }
    }

    pub fn user_query(&self) -> &str {
        &self.user_query
    }

    pub fn api_key(&self) -> LLMProviderAPIKeys {
        self.api_key.clone()
    }

    pub fn llm_provider(&self) -> LLMProvider {
        self.llm_provider.clone()
    }

    pub fn model(&self) -> &LLMType {
        &self.llm_type
    }

    pub fn user_context(&self) -> &UserContext {
        &self.user_context
    }

    pub fn remove_user_context(self) -> UserContext {
        self.user_context
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolUtilityRequest {
    user_query: String,
    definitions_alredy_present: Vec<String>,
    fs_file_path: String,
    fs_file_content: String,
    selection_range: Range,
    language: String,
    llm_type: LLMType,
    llm_provider: LLMProvider,
    api_key: LLMProviderAPIKeys,
    user_context: UserContext,
}

impl CodeSymbolUtilityRequest {
    pub fn new(
        user_query: String,
        definitions_alredy_present: Vec<String>,
        fs_file_path: String,
        fs_file_content: String,
        selection_range: Range,
        language: String,
        llm_type: LLMType,
        llm_provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
        user_context: UserContext,
    ) -> Self {
        Self {
            user_query,
            definitions_alredy_present,
            fs_file_content,
            fs_file_path,
            selection_range,
            language,
            llm_provider,
            llm_type,
            api_key,
            user_context,
        }
    }

    pub fn definitions(&self) -> &[String] {
        self.definitions_alredy_present.as_slice()
    }

    pub fn selection_range(&self) -> &Range {
        &self.selection_range
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn fs_file_path(&self) -> &str {
        &self.fs_file_path
    }

    pub fn file_content(&self) -> &str {
        &self.fs_file_content
    }

    pub fn user_query(&self) -> &str {
        &self.user_query
    }

    pub fn model(&self) -> LLMType {
        self.llm_type.clone()
    }

    pub fn provider(&self) -> LLMProvider {
        self.llm_provider.clone()
    }

    pub fn api_key(&self) -> LLMProviderAPIKeys {
        self.api_key.clone()
    }

    pub fn user_context(self) -> UserContext {
        self.user_context
    }
}

/// This request will give us code symbols and additional questions
/// we want to ask them before making our edits
/// This way we can ensure that the world moves to the state we are interested in
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolToAskQuestionsRequest {
    history: String,
    symbol_identifier: String,
    fs_file_path: String,
    code_above: Option<String>,
    code_below: Option<String>,
    code_in_selection: String,
    llm_type: LLMType,
    provider: LLMProvider,
    api_key: LLMProviderAPIKeys,
    query: String,
}

impl CodeSymbolToAskQuestionsRequest {
    pub fn new(
        history: String,
        symbol_identifier: String,
        fs_file_path: String,
        code_above: Option<String>,
        code_below: Option<String>,
        code_in_selection: String,
        llm_type: LLMType,
        provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
        query: String,
    ) -> Self {
        Self {
            history,
            symbol_identifier,
            fs_file_path,
            code_above,
            code_below,
            code_in_selection,
            llm_type,
            provider,
            api_key,
            query,
        }
    }

    pub fn history(&self) -> &str {
        &self.history
    }

    pub fn symbol_identifier(&self) -> &str {
        &self.symbol_identifier
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolImportantRequest {
    // if we have any symbol identifier here which we are focussing on, we keep
    // track of that here, if there is no history then we do not care about it.
    symbol_identifier: Option<String>,
    // history here consists of the symbols which we have followed to get to this
    // place
    history: Vec<String>,
    fs_file_path: String,
    fs_file_content: String,
    selection_range: Range,
    language: String,
    llm_type: LLMType,
    llm_provider: LLMProvider,
    api_key: LLMProviderAPIKeys,
    // this at the start will be the user query
    query: String,
}

impl CodeSymbolImportantRequest {
    pub fn new(
        symbol_identifier: Option<String>,
        history: Vec<String>,
        fs_file_path: String,
        fs_file_content: String,
        selection_range: Range,
        llm_type: LLMType,
        llm_provider: LLMProvider,
        api_key: LLMProviderAPIKeys,
        language: String,
        query: String,
    ) -> Self {
        Self {
            symbol_identifier,
            history,
            fs_file_path,
            fs_file_content,
            selection_range,
            llm_type,
            llm_provider,
            api_key,
            query,
            language,
        }
    }

    pub fn symbol_identifier(&self) -> Option<&str> {
        self.symbol_identifier.as_deref()
    }

    pub fn model(&self) -> &LLMType {
        &self.llm_type
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn file_path(&self) -> &str {
        &self.fs_file_path
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn content(&self) -> &str {
        &self.fs_file_content
    }

    pub fn range(&self) -> &Range {
        &self.selection_range
    }

    pub fn api_key(&self) -> &LLMProviderAPIKeys {
        &self.api_key
    }

    pub fn provider(&self) -> &LLMProvider {
        &self.llm_provider
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolWithThinking {
    code_symbol: String,
    thinking: String,
    file_path: String,
}

impl CodeSymbolWithThinking {
    pub fn new(code_symbol: String, thinking: String, file_path: String) -> Self {
        Self {
            code_symbol,
            thinking,
            file_path,
        }
    }

    pub fn code_symbol(&self) -> &str {
        &self.code_symbol
    }

    pub fn thinking(&self) -> &str {
        &self.thinking
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolWithSteps {
    code_symbol: String,
    steps: Vec<String>,
    is_new: bool,
    file_path: String,
}

impl CodeSymbolWithSteps {
    pub fn new(code_symbol: String, steps: Vec<String>, is_new: bool, file_path: String) -> Self {
        Self {
            code_symbol,
            steps,
            is_new,
            file_path,
        }
    }

    pub fn code_symbol(&self) -> &str {
        &self.code_symbol
    }

    pub fn steps(&self) -> &[String] {
        self.steps.as_slice()
    }

    pub fn is_new(&self) -> bool {
        self.is_new
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeSymbolImportantResponse {
    symbols: Vec<CodeSymbolWithThinking>,
    ordered_symbols: Vec<CodeSymbolWithSteps>,
}

impl CodeSymbolImportantResponse {
    pub fn new(
        symbols: Vec<CodeSymbolWithThinking>,
        ordered_symbols: Vec<CodeSymbolWithSteps>,
    ) -> Self {
        Self {
            symbols,
            ordered_symbols,
        }
    }

    pub fn symbols(&self) -> &[CodeSymbolWithThinking] {
        self.symbols.as_slice()
    }

    pub fn remove_symbols(self) -> Vec<CodeSymbolWithThinking> {
        self.symbols
    }

    pub fn ordered_symbols(&self) -> &[CodeSymbolWithSteps] {
        self.ordered_symbols.as_slice()
    }
}

#[async_trait]
pub trait CodeSymbolImportant {
    async fn get_important_symbols(
        &self,
        code_symbols: CodeSymbolImportantRequest,
    ) -> Result<CodeSymbolImportantResponse, CodeSymbolError>;

    async fn context_wide_search(
        &self,
        context_wide_search: CodeSymbolImportantWideSearch,
    ) -> Result<CodeSymbolImportantResponse, CodeSymbolError>;

    async fn gather_utility_symbols(
        &self,
        utility_symbol_request: CodeSymbolUtilityRequest,
    ) -> Result<CodeSymbolImportantResponse, CodeSymbolError>;

    async fn symbols_to_ask_questions(
        &self,
        request: CodeSymbolToAskQuestionsRequest,
    ) -> Result<(), CodeSymbolError>;
}

// implement passing in just the user context and getting the data back
// we have to implement a wider search over here and grab all the symbols and
// then further refine it and set out agents to work on them
// let's see how that works out (would be interesting)
