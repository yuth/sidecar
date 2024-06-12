//! Identifier here represents how the code will look like if we have metadata and the
//! location for it
//! We can also use the tools along with this symbol to traverse the code graph

use std::{collections::HashSet, sync::Arc};

use derivative::Derivative;
use futures::{lock::Mutex, stream, StreamExt};
use llm_client::{
    clients::types::LLMType,
    provider::{LLMProvider, LLMProviderAPIKeys},
};

use crate::{
    chunking::{text_document::Range, types::OutlineNodeContent},
    user_context::types::UserContext,
};

use super::{
    errors::SymbolError,
    events::{
        edit::{SymbolToEdit, SymbolToEditRequest},
        types::SymbolEvent,
    },
    tool_box::ToolBox,
    tool_properties::ToolProperties,
    types::SymbolEventRequest,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LLMProperties {
    llm: LLMType,
    provider: LLMProvider,
    api_key: LLMProviderAPIKeys,
}

impl LLMProperties {
    pub fn new(llm: LLMType, provider: LLMProvider, api_keys: LLMProviderAPIKeys) -> Self {
        Self {
            llm,
            provider,
            api_key: api_keys,
        }
    }

    pub fn llm(&self) -> &LLMType {
        &self.llm
    }

    pub fn provider(&self) -> &LLMProvider {
        &self.provider
    }

    pub fn api_key(&self) -> &LLMProviderAPIKeys {
        &self.api_key
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct Snippet {
    range: Range,
    symbol_name: String,
    fs_file_path: String,
    content: String,
    language: Option<String>,
    // this represents completely a snippet of code which is a logical symbol
    // so a class here will have the complete node (along with all the function inside it),
    // and if its a function then this will be the funciton by itself
    outline_node_content: OutlineNodeContent,
}

impl Snippet {
    pub fn new(
        symbol_name: String,
        range: Range,
        fs_file_path: String,
        content: String,
        outline_node_content: OutlineNodeContent,
    ) -> Self {
        Self {
            symbol_name,
            range,
            fs_file_path,
            content,
            language: None,
            outline_node_content,
        }
    }

    pub fn is_potential_match(&self, range: &Range, fs_file_path: &str, is_outline: bool) -> bool {
        if &self.range == range && self.fs_file_path == fs_file_path {
            if is_outline {
                if self.outline_node_content.is_class_type() {
                    true
                } else {
                    // TODO(skcd): This feels wrong, but I am not sure yet
                    false
                }
            } else {
                true
            }
        } else {
            false
        }
    }

    // TODO(skcd): Fix the language over here and make it not None
    pub fn language(&self) -> String {
        self.language.clone().unwrap_or("".to_owned()).to_owned()
    }

    pub fn file_path(&self) -> &str {
        &self.fs_file_path
    }

    pub fn range(&self) -> &Range {
        &self.range
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn to_xml(&self) -> String {
        let name = &self.symbol_name;
        let file_path = self.file_path();
        let start_line = self.range().start_line();
        let end_line = self.range().end_line();
        let content = self.content();
        let language = self.language();
        format!(
            r#"<name>
{name}
</name>
<file_path>
{file_path}:{start_line}-{end_line}
</file_path>
<content>
```{language}
{content}
```
</content>"#
        )
        .to_owned()
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, serde::Deserialize, serde::Serialize)]
pub struct SymbolIdentifier {
    symbol_name: String,
    fs_file_path: Option<String>,
}

impl SymbolIdentifier {
    pub fn new_symbol(symbol_name: &str) -> Self {
        Self {
            symbol_name: symbol_name.to_owned(),
            fs_file_path: None,
        }
    }

    pub fn fs_file_path(&self) -> Option<String> {
        self.fs_file_path.clone()
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }

    pub fn with_file_path(symbol_name: &str, fs_file_path: &str) -> Self {
        Self {
            symbol_name: symbol_name.to_owned(),
            fs_file_path: Some(fs_file_path.to_owned()),
        }
    }
}

#[derive(Debug)]
pub struct SnippetReRankInformation {
    idx: usize,
    range: Range,
    fs_file_path: String,
    is_outline: bool,
}

impl SnippetReRankInformation {
    pub fn new(idx: usize, range: Range, fs_file_path: String) -> Self {
        Self {
            idx,
            range,
            fs_file_path,
            is_outline: false,
        }
    }

    pub fn idx(&self) -> usize {
        self.idx
    }

    pub fn range(&self) -> &Range {
        &self.range
    }

    pub fn fs_file_path(&self) -> &str {
        &self.fs_file_path
    }

    pub fn is_outline(&self) -> bool {
        self.is_outline
    }

    pub fn set_is_outline(mut self) -> Self {
        self.is_outline = true;
        self
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct MechaCodeSymbolThinking {
    symbol_name: String,
    steps: Mutex<Vec<String>>,
    is_new: bool,
    file_path: String,
    snippet: Mutex<Option<Snippet>>,
    // this contains all the implementations, if there were children before
    // for example: functions inside the class, they all get flattened over here
    implementations: Mutex<Vec<Snippet>>,
    // This can be updated on the fly when the user provides more context
    // We can think of this as a long term storage
    provided_user_context: UserContext,
    // The tool box which contains all the tools necessary
    #[derivative(Debug = "ignore")]
    tool_box: Arc<ToolBox>,
}

impl MechaCodeSymbolThinking {
    pub fn new(
        symbol_name: String,
        steps: Vec<String>,
        is_new: bool,
        file_path: String,
        snippet: Option<Snippet>,
        implementations: Vec<Snippet>,
        provided_user_context: UserContext,
        tool_box: Arc<ToolBox>,
    ) -> Self {
        Self {
            symbol_name,
            steps: Mutex::new(steps),
            is_new,
            file_path,
            snippet: Mutex::new(snippet),
            implementations: Mutex::new(implementations),
            provided_user_context,
            tool_box,
        }
    }

    pub fn symbol_name(&self) -> &str {
        &self.symbol_name
    }

    // we need to find the snippet in the code symbol in the file we are interested
    // in and then use that for providing answers
    pub async fn find_snippet_and_create(
        symbol_name: &str,
        steps: Vec<String>,
        file_path: &str,
        provided_user_context: UserContext,
        tools: Arc<ToolBox>,
        request_id: &str,
    ) -> Option<Self> {
        let snippet_maybe = tools
            .find_snippet_for_symbol(file_path, symbol_name, request_id)
            .await;
        match snippet_maybe {
            Ok(snippet) => Some(MechaCodeSymbolThinking::new(
                symbol_name.to_owned(),
                steps,
                false,
                file_path.to_owned(),
                Some(snippet),
                vec![],
                provided_user_context,
                tools,
            )),
            Err(_) => None,
        }
    }

    pub fn user_context(&self) -> &UserContext {
        &self.provided_user_context
    }

    // potentital issue here is that the ranges might change after an edit
    // has been made, we have to be careful about that, for now we ball
    pub async fn find_symbol_to_edit(
        &self,
        range: &Range,
        fs_file_path: &str,
        is_outline: bool,
    ) -> Option<Snippet> {
        if let Some(snippet) = self.snippet.lock().await.as_ref() {
            if snippet.is_potential_match(range, fs_file_path, is_outline) {
                return Some(snippet.clone());
            }
        }
        // now we look at the implementations and try to find the potential match
        // over here
        self.implementations
            .lock()
            .await
            .iter()
            .find(|snippet| snippet.is_potential_match(range, fs_file_path, is_outline))
            .map(|snippet| snippet.clone())
    }

    /// This finds the sub-symbol which we want to probe
    /// The sub-symbol can be a function inside the class or a identifier in
    /// the class if needs be
    pub async fn find_sub_symbol_in_range(
        &self,
        range: &Range,
        fs_file_path: &str,
        request_id: &str,
    ) -> Result<String, SymbolError> {
        let file_open_result = self
            .tool_box
            .file_open(fs_file_path.to_owned(), request_id)
            .await?;
        let _ = self
            .tool_box
            .force_add_document(
                fs_file_path,
                file_open_result.contents_ref(),
                file_open_result.language(),
            )
            .await;
        let outline_node = self
            .tool_box
            .get_outline_nodes_grouped(fs_file_path)
            .await
            .ok_or(SymbolError::OutlineNodeNotFound(fs_file_path.to_owned()))?
            // Now we look inside the outline nodes and try to find the ones which contains this range
            // and then we will look into the children of it
            .into_iter()
            .filter(|outline_node| outline_node.range().contains_check_line(range))
            .next()
            .ok_or(SymbolError::NoOutlineNodeSatisfyPosition)?;
        let possible_child_node = outline_node
            .children()
            .into_iter()
            .find(|child_node| child_node.range().contains_check_line(range));
        if let Some(child_node) = possible_child_node {
            Ok(child_node.name().to_owned())
        } else {
            Ok(outline_node.name().to_owned())
        }
    }

    pub async fn find_symbol_in_range(&self, range: &Range, fs_file_path: &str) -> Option<String> {
        if let Some(snippet) = self.snippet.lock().await.as_ref() {
            if snippet.range.contains(range) && snippet.fs_file_path == fs_file_path {
                return Some(snippet.symbol_name.to_owned());
            }
        }
        self.implementations
            .lock()
            .await
            .iter()
            .find(|snippet| {
                if snippet.range.contains(range) && snippet.fs_file_path == fs_file_path {
                    true
                } else {
                    false
                }
            })
            .map(|snippet| snippet.symbol_name.to_owned())
    }

    pub async fn steps(&self) -> Vec<String> {
        println!(
            "mecha_code_symbol_thinking::steps::being({})",
            &self.symbol_name()
        );
        let results = self
            .steps
            .lock()
            .await
            .iter()
            .map(|step| step.to_owned())
            .collect::<Vec<_>>();
        println!(
            "mecha_code_symbol_thinking::steps::end({})",
            &self.symbol_name(),
        );
        results
    }

    pub fn is_new(&self) -> bool {
        self.is_new
    }

    pub fn to_symbol_identifier(&self) -> SymbolIdentifier {
        if self.is_new {
            SymbolIdentifier::new_symbol(&self.symbol_name)
        } else {
            SymbolIdentifier::with_file_path(&self.symbol_name, &self.file_path)
        }
    }

    pub async fn set_snippet(&self, snippet: Snippet) {
        let mut snippet_inside = self.snippet.lock().await;
        *snippet_inside = Some(snippet);
    }

    pub async fn is_snippet_present(&self) -> bool {
        self.snippet.lock().await.is_some()
    }

    pub async fn get_snippet(&self) -> Option<Snippet> {
        self.snippet.lock().await.clone()
    }

    pub async fn add_step(&self, step: &str) {
        self.steps.lock().await.push(step.to_owned());
    }

    pub fn fs_file_path(&self) -> &str {
        &self.file_path
    }

    pub async fn add_implementation(&self, implementation: Snippet) {
        self.implementations.lock().await.push(implementation);
    }

    pub async fn get_implementations(&self) -> Vec<Snippet> {
        let mut implementations = self
            .implementations
            .lock()
            .await
            .iter()
            .map(|snippet| snippet.clone())
            .collect::<Vec<_>>();
        println!(
            "mecha_code_symbol_thinking::get_implementations::get_snippet({})::get_implementations_len({})",
            &self.symbol_name(),
            implementations.len(),
        );
        let self_implementation = self.get_snippet().await;
        if let Some(snippet) = self_implementation {
            if !implementations.iter().any(|implementation| {
                implementation
                    .range()
                    .check_equality_without_byte(snippet.range())
                    && &implementation.fs_file_path == &snippet.fs_file_path
            }) {
                implementations.push(snippet);
            }
        }
        implementations
    }

    pub async fn set_implementations(&self, snippets: Vec<Snippet>) {
        let mut implementations = self.implementations.lock().await;
        *implementations = snippets;
    }

    /// Initial request follows the following flow:
    /// - COT + follow-along questions for any other symbols which might even lead to edits
    /// - Reranking the snippets for the symbol
    /// - Edit the current symbol
    pub async fn initial_request(
        &self,
        tool_box: Arc<ToolBox>,
        original_request: &str,
        llm_properties: LLMProperties,
        request_id: String,
        tool_properties: &ToolProperties,
    ) -> Result<SymbolEventRequest, SymbolError> {
        println!(
            "mecha_code_symbol_thinking::symbol_name({})",
            self.symbol_name()
        );
        let request_id_ref = &request_id;
        println!(
            "mecha_code_symbol_thinking::steps_end({})",
            self.symbol_name()
        );
        if self.is_snippet_present().await {
            // This is what we are trying to figure out
            // the idea representation here will be in the form of
            // now that we have added the snippets, we can ask the llm to rerank
            // the implementation snippets and figure out which to edit
            // once we have which to edit, we can then go to the references and keep
            // going from there whichever the LLM thinks is important for maintaining
            // the overall structure of the query
            // we also insert our own snipet into this
            // re-ranking for a complete symbol looks very different
            // we have to carefully craft the prompt in such a way that all the important
            // details are laid out properly
            // if its a class we call it a class, and if there are functions inside
            // it we call them out in a section, check how symbols are implemented
            // for a given LLM somewhere in the code
            // we have the text for all the snippets which are part of the class
            // there will be some here which will be the class definition and some
            // which are not part of it
            // so we use the ones which are part of the class defintion and name it
            // specially, so we can use it
            // struct A {....} is a special symbol
            // impl A {....} is also special and we show the symbols inside it one by
            // one for each function and in the order of they occur in the file
            // once we have the response we can set the agent to task on each of these snippets

            // TODO(skcd): We want to send this request for reranking
            // and get back the snippet indexes
            // and then we parse it back from here to get back to the symbol
            // we are interested in
            println!(
                "mecha_code_symbol_thinking::llm_request::start({})",
                self.symbol_name()
            );
            if let Some((ranked_xml_list, reverse_lookup)) = self.to_llm_request(&request_id).await
            {
                let is_too_long = if reverse_lookup.len() > 100 {
                    true
                } else {
                    false
                };
                // if the input to search for is too long, we change dynamically
                // to gemini-pro.1.5-flash
                let llm_properties_for_filtering = if is_too_long {
                    tool_properties
                        .get_swe_bench_reranking_llm()
                        .unwrap_or(llm_properties.clone())
                } else {
                    llm_properties
                };
                // now we send it over to the LLM and register as a rearank operation
                // and then ask the llm to reply back to us
                println!(
                    "mecha_code_symbol_thinking::filter_code_snippets_in_symbol_for_editing::start({})",
                    self.symbol_name(),
                );
                println!(
                    "mecha_code_symbol_thinking::filter_code_snippets_in_symbol_for_editing::ranked_xml_list({})\n{}",
                    self.symbol_name(),
                    &ranked_xml_list,
                );
                let filtered_list = dbg!(
                    tool_box
                        .filter_code_snippets_in_symbol_for_editing(
                            ranked_xml_list,
                            original_request.to_owned(),
                            llm_properties_for_filtering.llm().clone(),
                            llm_properties_for_filtering.provider().clone(),
                            llm_properties_for_filtering.api_key().clone(),
                            &request_id,
                        )
                        .await
                )?;

                // now we take this filtered list and try to generate back and figure out
                // the ranges which need to be edited
                let code_to_edit_list = filtered_list.code_to_edit_list();
                // we use this to map it back to the symbols which we should
                // be editing and then send those are requests to the hub
                // which will forward it to the right symbol
                let sub_symbols_to_edit = stream::iter(reverse_lookup)
                    .filter_map(|reverse_lookup| async move {
                        let idx = reverse_lookup.idx();
                        let range = reverse_lookup.range();
                        let fs_file_path = reverse_lookup.fs_file_path();
                        let outline = reverse_lookup.is_outline();
                        let found_reason_to_edit = code_to_edit_list
                            .snippets()
                            .into_iter()
                            .find(|snippet| snippet.id() == idx)
                            .map(|snippet| {
                                let reason_to_edit = snippet.reason_to_edit().to_owned();
                                format!(
                                    r#"Original user query:
{original_request}

Reason to edit:
{reason_to_edit}"#
                                )
                            });
                        match found_reason_to_edit {
                            Some(reason) => {
                                // TODO(skcd): We need to get the sub-symbol over
                                // here instead of the original symbol name which
                                // would not work
                                let symbol_in_range = self
                                    .find_sub_symbol_in_range(range, fs_file_path, request_id_ref)
                                    .await;
                                if let Ok(symbol) = symbol_in_range {
                                    Some(SymbolToEdit::new(
                                        symbol,
                                        range.clone(),
                                        fs_file_path.to_owned(),
                                        vec![reason],
                                        outline,
                                    ))
                                } else {
                                    None
                                }
                            }
                            None => None,
                        }
                    })
                    .collect::<Vec<_>>()
                    .await;

                // The idea with the edit requests is that the symbol agent
                // will send this over and then act on it by itself
                // this case is peculiar cause we are editing our own state
                // so we have to think about what that will look like for the agent
                // should we start working on it just at that point, or send it over
                // and keep a tag of the request we are making?
                Ok(SymbolEventRequest::new(
                    self.to_symbol_identifier(),
                    SymbolEvent::Edit(SymbolToEditRequest::new(
                        sub_symbols_to_edit,
                        self.to_symbol_identifier(),
                    )),
                    tool_properties.clone(),
                ))
            } else {
                todo!("what do we do over here")
            }
        } else {
            // we have to figure out the location for this symbol and understand
            // where we want to put this symbol at
            // what would be the best way to do this?
            // should we give the folder overview and then ask it
            // or assume that its already written out
            todo!("figure out what to do here");
        }
    }

    // We return an Option here because the symbol might not be present over here
    pub async fn get_symbol_content(&self) -> Option<Vec<String>> {
        let snippet_maybe = {
            self.snippet
                .lock()
                .await
                .as_ref()
                .map(|snippet| snippet.clone())
        };
        if let Some(snippet) = snippet_maybe {
            println!(
                "mecha_code_symbol_thinking::get_symbol_content::symbol_as_ref({})",
                &self.symbol_name()
            );
            let is_function = snippet
                .outline_node_content
                .outline_node_type()
                .is_function();
            let is_definition_assignment = snippet
                .outline_node_content
                .outline_node_type()
                .is_definition_assigument();
            if is_function || is_definition_assignment {
                let content = snippet.outline_node_content.content();
                let file_path = snippet.outline_node_content.fs_file_path();
                Some(vec![format!(
                    r#"<file_path>
{file_path}
</file_path>
<code_symbol>
{content}
</code_symbol>"#
                )])
            } else {
                let implementations = self.get_implementations().await;
                Some(
                    implementations
                        .into_iter()
                        .map(|implementation| {
                            let file_path = implementation.file_path();
                            let content = implementation.content();
                            format!(
                                r#"<file_path>
{file_path}
</file_path>
<code_symbol>
{content}
</code_symbol>"#
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                // This is a class, so over here we have to grab all the implementations
                // as well as the current snippet and then send that over
            }
        } else {
            None
        }
    }

    // To xml is a common way to say that the data object implements a way to be
    // written in a xml which is a standard way to represent it for a LLM
    // TODO(skcd): How do we get the symbols which need to be edited here
    // properly, can we ask the llm to put it out properly or we ask it for the section
    // index
    // in which case that might work with the caveat being that if the LLM gets confused
    // we will get a big threshold, another way would be that we ask the llm to also
    // reply in symbols and the indexes as well
    // we have to keep a mapping between the snippets and the indexes we are using
    // that's the hard part
    // we can reconstruct if nothing changes in between which is the initial case
    // anyways but might not be the case always
    // combining both would be better
    // we also need a mapping back here which will help us understand which snippet
    // to look at, the structure I can come up with is something like this:
    // idx -> (Range + FS_FILE_PATH + is_outline)
    // fin
    pub async fn to_llm_request(
        &self,
        request_id: &str,
    ) -> Option<(String, Vec<SnippetReRankInformation>)> {
        let snippet_maybe = {
            // take owernship of the snippet over here
            self.snippet
                .lock()
                .await
                .as_ref()
                .map(|snippet| snippet.clone())
        };
        if let Some(snippet) = snippet_maybe {
            println!(
                "mecha_code_symbol_thinking::to_llm_request::symbol_as_ref({})",
                &self.symbol_name()
            );
            let is_function = snippet
                .outline_node_content
                .outline_node_type()
                .is_function();
            let is_definition_assignment = snippet
                .outline_node_content
                .outline_node_type()
                .is_definition_assigument();
            if is_function || is_definition_assignment {
                let function_body = snippet.to_xml();
                Some((
                    format!(
                        r#"<rerank_entry>
<id>
0
</id>
{function_body}
</rerank_entry>"#
                    ),
                    vec![SnippetReRankInformation::new(
                        0,
                        snippet.range.clone(),
                        snippet.fs_file_path.to_owned(),
                    )],
                ))
            } else {
                // and now we have the other symbols which might be a mix of the following
                // functions
                // class implementations
                // one of the problems we hvae have here is that we have to show
                // the llm all these sections and then show the llm on how to edit them
                // this is the most interesting part since we do know what the implementation
                // block looks like with the functions removed, we can use huristics
                // to fix it or expose it as part of the outline nodes
                let implementations = self.get_implementations().await;
                let snippets_ref = implementations.iter().collect::<Vec<_>>();
                println!("mecha_code_symbol_thinking::to_llm_request::class_implementations::symbol({}):implementations_len({})", &self.symbol_name(), snippets_ref.len());
                let mut outline_nodes = vec![];
                for snippet in snippets_ref.iter() {
                    let outline_node = self
                        .tool_box
                        .get_outline_node_from_snippet(snippet, request_id)
                        .await;
                    if let Ok(outline_node) = outline_node {
                        outline_nodes.push(outline_node);
                    }
                }

                let outline_nodes_vec = outline_nodes
                    .into_iter()
                    .map(|outline_node| outline_node.consume_all_outlines())
                    .flatten()
                    .collect::<Vec<_>>();
                // Snippets here for class hide the functions, so we want to get
                // the outline node again over here and pass that back to the LLM
                let class_implementations = outline_nodes_vec
                    .iter()
                    .filter(|outline_node| outline_node.is_class_type())
                    .collect::<Vec<_>>();
                let functions = outline_nodes_vec
                    .iter()
                    .filter(|outline_node| outline_node.is_function_type())
                    .collect::<Vec<_>>();
                println!("mecha_code_symbol_thinking::to_llm_request::class_implementations::symbol({}):class_len({})", &self.symbol_name(), class_implementations.len());
                println!(
                    "mecha_code_symbol_thinking::to_llm_request::functions::symbol({})::function_len({})",
                    &self.symbol_name(),
                    functions.len(),
                );
                let mut covered_function_idx: HashSet<usize> = Default::default();
                let class_covering_functions = class_implementations
                    .into_iter()
                    .map(|class_implementation| {
                        let class_range = class_implementation.range();
                        let class_file_path = class_implementation.fs_file_path();
                        let filtered_functions = functions
                            .iter()
                            .enumerate()
                            .filter_map(|(idx, function)| {
                                if class_range.contains(function.range())
                                    && function.fs_file_path() == class_file_path
                                {
                                    covered_function_idx.insert(idx);
                                    Some(function)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>();
                        let class_non_overlap = class_implementation.get_non_overlapping_content(
                            filtered_functions
                                .iter()
                                .map(|filtered_function| filtered_function.range())
                                .collect::<Vec<_>>()
                                .as_slice(),
                        );
                        (class_implementation, filtered_functions, class_non_overlap)
                    })
                    .collect::<Vec<(
                        &OutlineNodeContent,
                        Vec<&&OutlineNodeContent>,
                        Option<String>,
                    )>>();

                // now we will generate the code snippets over here
                // and give them a list
                // this list is a bit special cause it also has prefix in between
                // for some symbols
                let mut symbol_index = 0;
                // we are hedging on the fact that the agent will be able to pick
                // up the snippets properly, instead of going for the inner symbols
                // (kind of orthodox I know, the reason is that the starting part of
                // the symbol is also important and editable, so this approach should
                // in theory work)
                // ideally we will move it back to a range based edit later on
                let mut symbol_rerank_information = vec![];
                let symbol_list = class_covering_functions
                    .into_iter()
                    .map(|(class_snippet, functions, non_overlap_prefix)| {
                        let formatted_snippet = class_snippet.to_xml();
                        if class_snippet.is_class_definition() {
                            let definition = format!(
                                r#"<rerank_entry>
<id>
{symbol_index}
</id>
{formatted_snippet}
</rerank_entry>"#
                            );
                            symbol_rerank_information.push(SnippetReRankInformation::new(
                                symbol_index,
                                class_snippet.range().clone(),
                                class_snippet.fs_file_path().to_owned(),
                            ));
                            symbol_index = symbol_index + 1;
                            definition
                        } else {
                            let overlap = if let Some(non_overlap_prefix) = non_overlap_prefix {
                                let file_path = class_snippet.fs_file_path();
                                let overlapp_snippet = format!(
                                    r#"<rerank_entry>
<id>
{symbol_index}
</id>
<file_path>
{file_path}
</file_path>
<content>
{non_overlap_prefix}
</content>
</rerank_entry>"#
                                )
                                .to_owned();
                                symbol_rerank_information.push(
                                    SnippetReRankInformation::new(
                                        symbol_index,
                                        class_snippet.range().clone(),
                                        class_snippet.fs_file_path().to_owned(),
                                    )
                                    .set_is_outline(),
                                );
                                symbol_index = symbol_index + 1;
                                Some(overlapp_snippet)
                            } else {
                                None
                            };
                            let function_snippets = functions
                                .into_iter()
                                .map(|function| {
                                    let function_body = function.to_xml();
                                    let function_code_snippet = format!(
                                        r#"<rerank_entry>
<id>
{symbol_index}
</id>
{function_body}
</rerank_entry>"#
                                    );
                                    symbol_rerank_information.push(SnippetReRankInformation::new(
                                        symbol_index,
                                        function.range().clone(),
                                        function.fs_file_path().to_owned(),
                                    ));
                                    symbol_index = symbol_index + 1;
                                    function_code_snippet
                                })
                                .collect::<Vec<_>>()
                                .join("\n");

                            // now that we have the overlap we have to join it together
                            // with the functions
                            if let Some(overlap) = overlap {
                                format!(
                                    r#"{overlap}
{function_snippets}"#
                                )
                            } else {
                                function_snippets
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                // for functions which are inside trait boundaries we will do the following:
                // try to get the lines which are not covered by the functions from the outline
                // remove the } from the end of the string (always try and do class.end_line() - max(function.end_line()))
                // and then we put the functions, that way things turn out structured as we want
                // TODO(skcd): This will break in the future since we want to identify the property
                // identifiers, but for now this is completely fine
                // now for the functions which are not covered we will create separate prompts for them
                // cause those are not covered by any class implementation (which is suss...)
                // now we try to see which functions belong to a class
                let uncovered_functions = functions
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, snippet)| {
                        if !covered_function_idx.contains(&idx) {
                            Some(snippet)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                // we still have the uncovered functions which we want to sort
                // through
                let uncovered_functions = uncovered_functions
                    .into_iter()
                    .map(|uncovered_function| {
                        let formatted_content = uncovered_function.to_xml();
                        let llm_snippet = format!(
                            "<rerank_entry>
<id>
{symbol_index}
</id>
{formatted_content}
</rerank_entry>"
                        );
                        symbol_rerank_information.push(SnippetReRankInformation::new(
                            symbol_index,
                            uncovered_function.range().clone(),
                            uncovered_function.fs_file_path().to_owned(),
                        ));
                        symbol_index = symbol_index + 1;
                        llm_snippet
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some((
                    format!(
                        "<rerank_list>
{symbol_list}
{uncovered_functions}
</rerank_list>"
                    ),
                    symbol_rerank_information,
                ))
            }
        } else {
            None
        }
    }
}
