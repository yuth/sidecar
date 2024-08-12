//! Contains the symbol type and how its structred and what kind of operations we
//! can do with it, its a lot of things but the end goal is that each symbol in the codebase
//! can be represented by some entity, and that's what we are storing over here
//! Inside each symbol we also have the various implementations of it, which we always
//! keep track of and whenever a question is asked we forward it to all the implementations
//! and select the ones which are necessary.

use std::{collections::HashMap, sync::Arc};

use derivative::Derivative;
use futures::{future::Shared, stream, FutureExt, StreamExt};
use llm_client::{
    clients::types::LLMType,
    provider::{GoogleAIStudioKey, LLMProvider, LLMProviderAPIKeys},
};
use logging::parea::{PareaClient, PareaLogEvent};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::info;

use crate::{
    agentic::{
        symbol::{
            events::edit::SymbolToEditRequest,
            helpers::find_needle_position,
            identifier::Snippet,
            ui_event::{SymbolEventProbeRequest, SymbolEventSubStep, SymbolEventSubStepRequest},
        },
        tool::code_symbol::{
            important::{CodeSubSymbolProbingResult, CodeSymbolProbingSummarize},
            models::anthropic::AskQuestionSymbolHint,
        },
    },
    chunking::{
        text_document::{Position, Range},
        types::OutlineNode,
    },
};

use super::{
    errors::SymbolError,
    events::{
        edit::SymbolToEdit,
        initial_request::{InitialRequestData, SymbolEditedItem, SymbolRequestHistoryItem},
        probe::{SymbolToProbeHistory, SymbolToProbeRequest},
        types::{AskQuestionRequest, SymbolEvent},
    },
    identifier::{LLMProperties, MechaCodeSymbolThinking, SymbolIdentifier},
    tool_box::ToolBox,
    tool_properties::ToolProperties,
    ui_event::UIEventWithID,
};

const BUFFER_LIMIT: usize = 100;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolSubStepUpdate {
    sybmol: SymbolIdentifier,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolLocation {
    snippet: Snippet,
    symbol_identifier: SymbolIdentifier,
}

impl SymbolLocation {
    pub fn new(symbol_identifier: SymbolIdentifier, snippet: Snippet) -> Self {
        Self {
            snippet,
            symbol_identifier,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolEventRequest {
    symbol: SymbolIdentifier,
    event: SymbolEvent,
    tool_properties: ToolProperties,
}

impl SymbolEventRequest {
    pub fn event(&self) -> &SymbolEvent {
        &self.event
    }

    pub fn symbol(&self) -> &SymbolIdentifier {
        &self.symbol
    }

    pub fn remove_event(self) -> SymbolEvent {
        self.event
    }

    pub fn get_tool_properties(&self) -> &ToolProperties {
        &self.tool_properties
    }
}

impl SymbolEventRequest {
    pub fn new(
        symbol: SymbolIdentifier,
        event: SymbolEvent,
        tool_properties: ToolProperties,
    ) -> Self {
        Self {
            symbol,
            event,
            tool_properties,
        }
    }

    pub fn outline(symbol: SymbolIdentifier, tool_properties: ToolProperties) -> Self {
        Self {
            symbol,
            event: SymbolEvent::Outline,
            tool_properties,
        }
    }

    pub fn ask_question(
        symbol: SymbolIdentifier,
        question: String,
        tool_properties: ToolProperties,
    ) -> Self {
        Self {
            symbol,
            event: SymbolEvent::AskQuestion(AskQuestionRequest::new(question)),
            tool_properties,
        }
    }

    pub fn probe_request(
        symbol: SymbolIdentifier,
        request: SymbolToProbeRequest,
        tool_properties: ToolProperties,
    ) -> Self {
        Self {
            symbol,
            event: SymbolEvent::Probe(request),
            tool_properties,
        }
    }

    pub fn initial_request(
        symbol: SymbolIdentifier,
        original_user_query: String,
        request: String,
        // passing history to the symbols so we do not end up doing repeated work
        history: Vec<SymbolRequestHistoryItem>,
        tool_properties: ToolProperties,
        symbols_edited_list: Option<Vec<SymbolEditedItem>>,
        is_big_search: bool,
    ) -> Self {
        Self {
            symbol,
            event: SymbolEvent::InitialRequest(InitialRequestData::new(
                original_user_query,
                request,
                history,
                tool_properties.get_full_symbol_request(),
                symbols_edited_list,
                is_big_search,
            )),
            tool_properties,
        }
    }
}

#[derive(Debug)]
pub enum SymbolEventResponse {
    TaskDone(String),
}

impl SymbolEventResponse {
    pub fn to_string(self) -> String {
        match self {
            Self::TaskDone(reply) => reply,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EditedCodeSymbol {
    original_code: String,
    edited_code: String,
}

impl EditedCodeSymbol {
    pub fn new(original_code: String, edited_code: String) -> Self {
        Self {
            original_code,
            edited_code,
        }
    }
}

/// The symbol is going to spin in the background and keep working on things
/// is this how we want it to work???
/// ideally yes, cause its its own process which will work in the background
/// one of the keys things here is that we want this to be a arcable and clone friendly
/// since we are sending many such events to it at the same time
#[derive(Derivative)]
#[derivative(PartialEq, Eq, Debug, Clone)]
pub struct Symbol {
    symbol_identifier: SymbolIdentifier,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    hub_sender: UnboundedSender<(
        SymbolEventRequest,
        String,
        // we can await on the receiver
        tokio::sync::oneshot::Sender<SymbolEventResponse>,
    )>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    tools: Arc<ToolBox>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    // TODO(skcd): this is a skill issue right here
    // we do not want to clone so aggresively here, we should be able to work
    // with just references somehow if we were not mutating our state so much
    mecha_code_symbol: Arc<MechaCodeSymbolThinking>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    llm_properties: LLMProperties,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    ui_sender: UnboundedSender<UIEventWithID>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    tool_properties: ToolProperties,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    probe_questions_asked: Arc<Mutex<Vec<String>>>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    parea_client: PareaClient,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    probe_questions_handler:
        Arc<Mutex<HashMap<String, Shared<tokio::sync::oneshot::Receiver<Option<String>>>>>>,
    #[derivative(PartialEq = "ignore")]
    #[derivative(Hash = "ignore")]
    #[derivative(Debug = "ignore")]
    probe_questions_answer: Arc<Mutex<HashMap<String, Option<String>>>>,
}

impl Symbol {
    pub async fn new(
        symbol_identifier: SymbolIdentifier,
        mecha_code_symbol: MechaCodeSymbolThinking,
        // this can be used to talk to other symbols and get them
        // to act on certain things
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            String,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
        tools: Arc<ToolBox>,
        llm_properties: LLMProperties,
        ui_sender: UnboundedSender<UIEventWithID>,
        request_id: String,
        tool_properties: ToolProperties,
    ) -> Result<Self, SymbolError> {
        let symbol = Self {
            mecha_code_symbol: Arc::new(mecha_code_symbol),
            symbol_identifier: symbol_identifier.clone(),
            hub_sender,
            tools: tools.clone(),
            llm_properties,
            ui_sender,
            tool_properties,
            probe_questions_asked: Arc::new(Mutex::new(vec![])),
            parea_client: PareaClient::new(),
            probe_questions_handler: Arc::new(Mutex::new(HashMap::new())),
            probe_questions_answer: Arc::new(Mutex::new(HashMap::new())),
        };
        // grab the implementations of the symbol
        // TODO(skcd): We also have to grab the diagnostics and auto-start any
        // process which we might want to
        symbol
            .mecha_code_symbol
            .grab_implementations(tools, symbol_identifier, &request_id)
            .await?;
        Ok(symbol)
    }

    pub fn mecha_code_symbol(&self) -> Arc<MechaCodeSymbolThinking> {
        self.mecha_code_symbol.clone()
    }

    fn fs_file_path(&self) -> &str {
        self.mecha_code_symbol.fs_file_path()
    }

    fn symbol_name(&self) -> &str {
        self.mecha_code_symbol.symbol_name()
    }

    // find the name of the sub-symbol
    pub async fn find_subsymbol_in_range(
        &self,
        range: &Range,
        fs_file_path: &str,
    ) -> Option<String> {
        self.mecha_code_symbol
            .find_symbol_in_range(range, fs_file_path)
            .await
    }

    async fn get_outline(&self, request_id: String) -> Result<String, SymbolError> {
        // to grab the outline first we need to understand the definition snippet
        // of the node and then create it appropriately
        // First thing we want to do here is to find the symbols which are present
        // in the file and get the one which corresponds to us, once we have that
        // we go to all the implementations and grab them as well and generate
        // the outline which is required
        self.tools
            .outline_nodes_for_symbol(self.fs_file_path(), self.symbol_name(), &request_id)
            .await
    }

    async fn probe_request_handler(
        &self,
        request: SymbolToProbeRequest,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            String,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
        request_id: String,
    ) -> Result<String, SymbolError> {
        let original_request_id = request.original_request_id().to_owned();
        // First check the answer hashmap if we already have the answer, and take
        // the answer from there if it already exists
        {
            let answered_questions = self.probe_questions_answer.lock().await;
            if let Some(answer) = answered_questions.get(&original_request_id) {
                return match answer {
                    Some(answer) => Ok(answer.to_string()),
                    None => Err(SymbolError::CachedQueryFailed),
                };
            }
        }
        let receiver: Shared<tokio::sync::oneshot::Receiver<_>>;
        let sender: Option<tokio::sync::oneshot::Sender<_>>;
        {
            let mut ongoing_probe_requests = self.probe_questions_handler.lock().await;
            if let Some(receiver_present) = ongoing_probe_requests.get(&original_request_id) {
                println!(
                    "symbol::probe_request_handler::cache_hit::{}",
                    self.symbol_name()
                );
                receiver = receiver_present.clone();
                sender = None;
            } else {
                println!(
                    "symbol::probe_request_handler::cache_not_present::{}",
                    self.symbol_name(),
                );
                // then we want to call the the probe_request and put the receiver on our end
                let (sender_present, receiver_present) = tokio::sync::oneshot::channel();
                let shared_receiver = receiver_present.shared();
                let _ = ongoing_probe_requests
                    .insert(original_request_id.to_owned(), shared_receiver.clone());
                sender = Some(sender_present);
                receiver = shared_receiver;
            }
        }

        // If we have the sender with us, then we need to really perform the probe work
        // and then poll the receiver for the results
        if let Some(sender) = sender {
            println!(
                "symbol::probe_request_handler::probe_request::({})",
                self.symbol_name()
            );
            let result = self.probe_request(request, hub_sender, request_id).await;
            // update our answer hashmap before sending it over
            {
                let mut answered_question = self.probe_questions_answer.lock().await;
                match &result {
                    Ok(result) => {
                        answered_question
                            .insert(original_request_id.to_owned(), Some(result.to_string()));
                    }
                    Err(e) => {
                        println!("symbol::probe_request_handler::probe_request::answer::error::({})::({:?})", self.symbol_name(), e);
                        answered_question.insert(original_request_id.to_owned(), None);
                    }
                }
            }
            match result {
                Ok(result) => {
                    let _ = sender.send(Some(result));
                }
                Err(e) => {
                    let _ = sender.send(None);
                    Err(e)?;
                }
            };
        }
        println!(
            "symbol::probe_request_handler::probe_request::receiver_wait::({})",
            self.symbol_name()
        );
        let result = receiver.await;
        println!(
            "symbol::probe_request_handler::probe_request::receiver_finished::({})",
            self.symbol_name()
        );
        match result {
            Ok(Some(result)) => Ok(result),
            _ => Err(SymbolError::CachedQueryFailed),
        }
    }

    /// Probing works in the following way:
    /// - We run 2 queries in parallel first:
    /// - - check if we have enough to answer the user query
    /// - - if we have to look deeper into certain sections of the code in the symbol
    /// - If we have to look deeper into certain sections of the code, find the code snippets
    /// where we have to cmd-click and grab them (these are the next probe symbols)
    /// - shoot a request to these next probe symbols with all the questions we have
    /// - wait in parallel and then merge the answer together to answer the user query
    async fn probe_request(
        &self,
        request: SymbolToProbeRequest,
        hub_sender: UnboundedSender<(
            SymbolEventRequest,
            String,
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
        request_id: String,
    ) -> Result<String, SymbolError> {
        let original_request_id = request.original_request_id().to_owned();
        let original_request_id_ref = &original_request_id;
        // We want to keep track of all the probe requests which are going on for this symbol
        // and make sure that we are not repeating work which has already been done or is ongoing.
        let all_ongoing_probe_requests: Vec<String>;
        {
            let mut probe_requests = self.probe_questions_asked.lock().await;
            probe_requests.push(request.probe_request().to_owned());
            all_ongoing_probe_requests = probe_requests.clone();
        }
        let event_trace_id = uuid::Uuid::new_v4().to_string();
        // log all the probe requests which might be going on or are done for a given symbol
        let _ = self
            .parea_client
            .log_event(PareaLogEvent::new(
                format!("probe_request::{}", self.symbol_name()),
                event_trace_id.to_owned(),
                event_trace_id.to_owned(),
                all_ongoing_probe_requests
                    .into_iter()
                    .enumerate()
                    .map(|(idx, content)| (idx.to_string(), content))
                    .collect(),
            ))
            .await;
        let request_id_ref = &request_id;
        // we can do a cache check here if we already have the answer or are working
        // on the similar request
        // First we refresh our state over here
        self.refresh_state(request_id_ref.to_owned()).await;

        let history = request.history();
        let history_slice = request.history_slice();
        let original_request = request.original_request();
        let history_ref = &history;
        let query = request.probe_request();
        let symbol_name = self.mecha_code_symbol.symbol_name();
        let tool_properties_ref = &self.tool_properties;

        let _snippets = self.mecha_code_symbol.get_implementations().await;
        info!(event_name = "refresh_state", symbol_name = symbol_name,);
        // - sub-symbol selection for probing
        // TODO(skcd): We are sending full blocks of implementations over here
        // instead we should be splitting the outline node into parts and then
        // asking which section we want to probe and show that to the LLM
        // for example: struct A { fn something(); fn something_else(); }
        // should break down into struct A {fn something(); }, struct A{ fn something_else(); }
        // instead of being struct A {fn something(); fn something_else(); }

        // TODO(skcd): We run both the queries in parallel:
        // - ask if the symbol can answer the question
        // - or we chose the sub-symbol which we want to focus on
        println!(
            "symbol::probe_request::parallel_requests_start::({})",
            &self.symbol_name()
        );
        let (probe_sub_symbols, probe_deeper_or_enough) = tokio::join!(
            self.mecha_code_symbol.probe_sub_sybmols(
                query,
                self.llm_properties.clone(),
                request_id_ref.to_owned()
            ),
            self.mecha_code_symbol.probe_deeper_or_answer(
                query,
                self.llm_properties.clone(),
                request_id_ref.to_owned()
            ),
        );
        if let Ok(probe_deeper_or_enough) = probe_deeper_or_enough {
            if let Some(answer_user_query) = probe_deeper_or_enough.answer_user_query() {
                // we found the answer very early, so lets just return over here
                let _ = self.ui_sender.send(UIEventWithID::probe_answer_event(
                    request_id_ref.to_owned(),
                    self.symbol_identifier.clone(),
                    answer_user_query.to_owned(),
                ));
                println!(
                    "symbol::probe_request::answer_user_query::({})",
                    self.symbol_name()
                );
                return Ok(answer_user_query);
            }
        }

        // TODO(skcd): We are not getting the right question to ask to the sub-symbols
        // this is more of an observation about the sub-symbol related to our question
        let probe_sub_symbols = probe_sub_symbols?;
        println!(
            "symbol::probe_request::probe_sub_symbols::({})::len({})",
            self.symbol_name(),
            probe_sub_symbols.len()
        );
        let _ = self.ui_sender.send(UIEventWithID::sub_symbol_step(
            request_id_ref.to_owned(),
            SymbolEventSubStepRequest::new(
                self.symbol_identifier.clone(),
                SymbolEventSubStep::Probe(SymbolEventProbeRequest::SubSymbolSelection),
            ),
        ));
        println!("Sub symbol request: {:?}", &probe_sub_symbols);

        info!("sub symbol fetching done");

        // If we do end up doing this, we should just send the go-to-definition
        // request over here properly and ask the LLM to follow the symbol
        // the check at the start will stop the probe from going askew
        // and the ones which are required will always work out
        let snippet_to_symbols_to_follow = stream::iter(probe_sub_symbols)
            .map(|probe_sub_symbol| async move {
                println!(
                    "symbol::probe_request::probe_sub_symbols::({})({}@{})",
                    self.symbol_name(),
                    probe_sub_symbol.symbol_name(),
                    probe_sub_symbol.fs_file_path(),
                );
                let outline_node = self
                    .tools
                    .find_sub_symbol_to_probe_with_name(
                        self.symbol_name(),
                        &probe_sub_symbol,
                        request_id_ref,
                    )
                    .await;
                if let Ok(outline_node) = outline_node {
                    let snippet = Snippet::new(
                        probe_sub_symbol.symbol_name().to_owned(),
                        outline_node.range().clone(),
                        probe_sub_symbol.fs_file_path().to_owned(),
                        outline_node.content().to_owned(),
                        outline_node.clone(),
                    );
                    let response = self
                        .tools
                        // TODO(skcd): This seems okay for now, given at the context
                        // of sending the whole symbol before, instead of individual
                        // chunks, so if we send individual snippets here instead
                        // we could get rid of this LLM call over here
                        // [LLM:symbols_to_probe_questions]
                        .probe_deeper_in_symbol(
                            &snippet,
                            probe_sub_symbol.reason(),
                            history_ref,
                            query,
                            self.llm_properties.llm().clone(),
                            self.llm_properties.provider().clone(),
                            self.llm_properties.api_key().clone(),
                            request_id_ref,
                        )
                        .await;
                    Some((snippet, response))
                } else {
                    None
                }
            })
            .buffer_unordered(BUFFER_LIMIT)
            .collect::<Vec<_>>()
            .await;
        let _ = self.ui_sender.send(UIEventWithID::sub_symbol_step(
            request_id_ref.to_owned(),
            SymbolEventSubStepRequest::new(
                self.symbol_identifier.clone(),
                SymbolEventSubStep::Probe(SymbolEventProbeRequest::ProbeDeeperSymbol),
            ),
        ));

        println!(
            "Snippet to symbols to follow: {:?}",
            snippet_to_symbols_to_follow
        );

        let symbol_identifier_ref = &self.symbol_identifier;

        // Now for each of the go-to-definition we have to find the snippet and
        // the symbol it belongs to and send the request to the appropriate symbol
        // and let it answer the user query followed by the question which the
        // LLM itself will be asking
        let snippet_to_follow_with_definitions = stream::iter(
            snippet_to_symbols_to_follow
                .into_iter()
                .filter_map(|something| match something {
                    Some((snippet, Ok(response))) => Some((snippet, response)),
                    _ => None,
                }),
        )
        .map(|(snippet, response)| async move {
            let referred_snippet = &snippet;
            // we want to parse the reponse here properly and get back the
            // snippets which are the most important by going to their definitions
            let symbols_to_follow_list = response.symbol_list();
            let definitions_to_follow = stream::iter(symbols_to_follow_list)
                .map(|symbol_to_follow| async move {
                    println!(
                        "Go to definition using symbol: {:?} {} {} {}",
                        referred_snippet.range(),
                        symbol_to_follow.file_path(),
                        symbol_to_follow.line_content(),
                        symbol_to_follow.name()
                    );
                    // the definitions over here might be just the symbols themselves
                    // we have to make sure that there is no self-reference and the
                    // LLM helps push the world model forward
                    let definitions_for_snippet = self
                        .tools
                        .go_to_definition_using_symbol(
                            referred_snippet.range(),
                            symbol_to_follow.file_path(),
                            symbol_to_follow.line_content(),
                            symbol_to_follow.name(),
                            request_id_ref,
                        )
                        .await;
                    if let Ok(ref definitions_for_snippet) = &definitions_for_snippet {
                        println!(
                            "symbol::probe_request::go_to_definition::ui_event::({})",
                            self.symbol_name()
                        );
                        let _ = self.ui_sender.send(UIEventWithID::sub_symbol_step(
                            request_id_ref.to_owned(),
                            SymbolEventSubStepRequest::go_to_definition_request(
                                symbol_identifier_ref.clone(),
                                symbol_to_follow.file_path().to_owned(),
                                definitions_for_snippet.1.clone(),
                                symbol_to_follow.thinking().to_owned(),
                            ),
                        ));
                    }
                    if let Ok(definitions_for_snippet) = definitions_for_snippet {
                        Some((symbol_to_follow, definitions_for_snippet))
                    } else {
                        None
                    }
                })
                .buffer_unordered(BUFFER_LIMIT)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(|s| s)
                .collect::<Vec<_>>();
            // Now that we have this, we can ask the LLM to generate the next set of probe-queries
            // if required unless it already has the answer
            (snippet, definitions_to_follow)
        })
        // we go through the snippets one by one
        .buffered(100)
        .collect::<Vec<_>>()
        .await;

        println!(
            "Snippet to follow with definitions: {:?}",
            &snippet_to_follow_with_definitions
        );

        // We could be asking the same question to multiple definitions of the symbol
        // TODO(skcd): We should pass the history of the sub-symbol snippet of how we
        // are asking the question, since we are directly going for the go-to-definition
        let probe_results = stream::iter(snippet_to_follow_with_definitions.to_vec())
            .map(|(snippet, ask_question_symbol_hint)| async move {
                // A single snippet can have multiple go-to-definition-requests
                // Here we try to organise them in the form or outline node probe
                // requests
                let questions_with_definitions = ask_question_symbol_hint;
                // Now for the definition we want to follow, we have to do some deduplication
                // based on symbol identifier and the request we are getting.
                // since we might end up getting lot of requests to the same symbol, we need
                // to merge all the asks together.
                let mut question_to_outline_nodes = vec![];
                for (ask_question_hint, definition) in questions_with_definitions.into_iter() {
                    let definitions = definition.2;
                    let outline_nodes = stream::iter(definitions)
                        .map(|definition| async move {
                            let range = definition.0.range();
                            let fs_file_path = definition.0.file_path();
                            println!("symbol::probe_request::get_outline_node_for_range::position({}, {:?})", fs_file_path, range);
                            let outline_nodes = self
                                .tools
                                .get_outline_node_for_range(range, fs_file_path, &request_id_ref)
                                .await;
                            // log if the outline node is present for this go-to-definition
                            // location
                            match outline_nodes {
                                Ok(outline_node) => {
                                    println!(
                                        "symbol::probe_request::go_to_definition::success({})",
                                        self.symbol_name()
                                    );
                                    Some(outline_node)
                                }
                                Err(_e) => {
                                    println!(
                                        "symbol::probe_request::go_to_definition::failure({})", self.symbol_name()
                                    );
                                    None
                                },
                            }
                        })
                        .buffer_unordered(100)
                        .collect::<Vec<_>>()
                        .await
                        .into_iter()
                        .filter_map(|s| s)
                        .collect::<Vec<_>>();
                    if outline_nodes.is_empty() {
                        println!("symbol::probe_request::ask_question_hint::failed::no_outline_nodes::({})", self.symbol_name());
                    } else {
                        question_to_outline_nodes.push((snippet.clone(), ask_question_hint, outline_nodes));
                    }
                }
                question_to_outline_nodes
            })
            .buffer_unordered(100)
            .collect::<Vec<_>>()
            .await;

        // outline nodes to probe questions
        let mut outline_node_to_probe_question: HashMap<
            OutlineNode,
            Vec<(Snippet, AskQuestionSymbolHint)>,
        > = HashMap::new();
        probe_results.into_iter().flatten().for_each(
            |(snippet, ask_question_hint, outline_nodes)| {
                for outline_node in outline_nodes {
                    if let Some(questions) = outline_node_to_probe_question.get_mut(&outline_node) {
                        questions.push((snippet.clone(), ask_question_hint.clone()));
                    } else {
                        outline_node_to_probe_question.insert(
                            outline_node,
                            vec![(snippet.clone(), ask_question_hint.clone())],
                        );
                    }
                }
            },
        );

        // Now we can create the probe next query for these outline nodes:
        let implementations = self.mecha_code_symbol.get_implementations().await;
        let symbol_to_probe_request = stream::iter(
            outline_node_to_probe_question
                .into_iter()
                .filter_map(|(outline_node, questions)| {
                    // todo something else over here
                    // First we need to check if the outline node belongs to the current symbol
                    // in which case we need to do something special over here, to avoid having
                    // cyclic loops, for now we can even begin with logging them
                    let outline_node_range = outline_node.range();
                    let outline_node_fs_file_path = outline_node.fs_file_path();
                    if implementations.iter().any(|implementation| {
                        implementation
                            .range()
                            .contains_check_line(&outline_node_range)
                            && implementation.file_path() == outline_node_fs_file_path
                    }) {
                        println!(
                            "symbol::probe_request::same_symbol_probing::({})",
                            self.symbol_name()
                        );
                        None
                    } else {
                        Some((outline_node, questions))
                    }
                }),
        )
        .map(|(outline_node, linked_snippet_with_questions)| async move {
            // over here we are going to send over a request to the outline nodes
            // and ask them all the questions we have for them to answer
            let symbol_identifier =
                SymbolIdentifier::with_file_path(outline_node.name(), outline_node.fs_file_path());

            // TODO(skcd): Create a new question request here using the data
            // from the previous queries
            //             let questions = linked_snippet_with_questions
            //                 .iter()
            //                 .map(|(_, question)| question)
            //                 .collect::<Vec<_>>();
            //             let mut reason = questions
            //                 .into_iter()
            //                 .map(|question| question.thinking().to_owned())
            //                 .collect::<Vec<_>>()
            //                 .join("\n");
            //             reason = format!(
            //                 r"#<original_question>
            // {query}
            // </original_question>
            // <observation_about_symbol>
            // {reason}
            // </observation_about_symbol>#"
            //             );
            let mut history = history_slice.to_vec();
            history.push(SymbolToProbeHistory::new(
                self.symbol_name().to_owned(),
                self.fs_file_path().to_owned(),
                "".to_owned(),
                query.to_owned(),
            ));
            let history_str = history
                .to_vec()
                .into_iter()
                .map(|history| {
                    let symbol_name = history.symbol().to_owned();
                    let fs_file_path = history.fs_file_path().to_owned();
                    let question = history.question().to_owned();
                    format!(
                        r#"<symbol_name>
{symbol_name}
</symbol_name>
<fs_file_path>
{fs_file_path}
</fs_file_path>
<question_asked>
{question}
</question_asked>"#
                    )
                })
                .collect::<Vec<_>>();
            let request = self
                .tools
                .probe_query_generation_for_symbol(
                    self.symbol_name(),
                    outline_node.name(),
                    outline_node.fs_file_path(),
                    original_request,
                    history_str,
                    linked_snippet_with_questions,
                    self.llm_properties.clone(),
                )
                .await;
            match request {
                Ok(request) => {
                    // The history item is not formatted here properly
                    // we need to make sure that we are passing the highlights of the snippets
                    // from where we are asking the question
                    Some((
                        outline_node,
                        SymbolToProbeRequest::new(
                            symbol_identifier,
                            request,
                            original_request.to_owned(),
                            original_request_id_ref.to_owned(),
                            history,
                        ),
                    ))
                }
                Err(e) => {
                    println!(
                        "symbol::probe_request::probe_query_generation_for_symbol::({:?})",
                        e
                    );
                    None
                }
            }
        })
        .buffer_unordered(100)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(|s| s)
        .collect::<Vec<_>>();

        if symbol_to_probe_request.is_empty() {
            // TODO(skcd): if this is empty, then we should have our answer ready over here
            // we should probably ask the LLM to illcit an answer
            // marking this as a TODO
            // otherwise its a class and we might have to create a new symbol over here
            let all_contents = self
                .mecha_code_symbol
                .get_implementations()
                .await
                .into_iter()
                .map(|snippet| {
                    let file_path = snippet.file_path();
                    let content = snippet.content();
                    format!(
                        r#"<file_path>
{file_path}
</file_path>
<content>
{content}
</content>"#
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let response = self
                .tools
                .probe_try_hard_answer(
                    all_contents,
                    LLMProperties::new(
                        LLMType::GeminiPro,
                        LLMProvider::GoogleAIStudio,
                        LLMProviderAPIKeys::GoogleAIStudio(GoogleAIStudioKey::new(
                            "AIzaSyCMkKfNkmjF8rTOWMg53NiYmz0Zv6xbfsE".to_owned(),
                        )),
                    ),
                    original_request,
                    query,
                    &request_id_ref,
                )
                .await
                .unwrap_or("LLM error, please contact the developers".to_owned());
            let _ = self.ui_sender.send(UIEventWithID::probe_answer_event(
                request_id_ref.to_owned(),
                self.symbol_identifier.clone(),
                response.to_owned(),
            ));
            Ok(response.to_owned())
        } else {
            // send the requests over here to the symbol manager and then await
            // in parallel
            let hub_sender_ref = &hub_sender;
            // log all the symbols we are going to probe to parea
            let _ = self
                .parea_client
                .log_event(PareaLogEvent::new(
                    format!("probe_dependency_edges::{}", self.symbol_name()),
                    request_id.to_owned(),
                    request_id.to_owned(),
                    vec![
                        ("calling_node".to_owned(), self.symbol_name().to_owned()),
                        (
                            "edges".to_owned(),
                            symbol_to_probe_request
                                .iter()
                                .map(|(outline_node, _)| outline_node.name())
                                .collect::<Vec<_>>()
                                .join(" ,"),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ))
                .await;
            let responses = stream::iter(symbol_to_probe_request)
                .map(|(outline_node, symbol_to_probe_request)| async move {
                    let symbol_identifier = symbol_to_probe_request.symbol_identifier().clone();
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let _ = hub_sender_ref.clone().send((
                        SymbolEventRequest::probe_request(
                            symbol_identifier,
                            symbol_to_probe_request,
                            tool_properties_ref.clone(),
                        ),
                        uuid::Uuid::new_v4().to_string(),
                        sender,
                    ));
                    let response = receiver.await.map(|response| response.to_string());
                    match response {
                        Ok(response) => {
                            Some(CodeSubSymbolProbingResult::new(
                                outline_node.name().to_owned(),
                                outline_node.fs_file_path().to_owned(),
                                vec![response],
                                // Here we should have the outline and not the
                                // complete symbol over here
                                outline_node.content().content().to_owned(),
                            ))
                        }
                        Err(e) => {
                            println!(
                                "symbol::probe_request::sub_symbol::probe_result::error({:?})",
                                e
                            );
                            None
                        }
                    }
                })
                .buffer_unordered(100)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(|s| s)
                .collect::<Vec<_>>();

            // summarize the results over here properly
            let request = CodeSymbolProbingSummarize::new(
                query.to_owned(),
                history.to_owned(),
                self.mecha_code_symbol.symbol_name().to_owned(),
                self.get_outline(request_id_ref.to_owned()).await?,
                self.mecha_code_symbol.fs_file_path().to_owned(),
                responses,
                LLMType::GeminiPro,
                LLMProvider::GoogleAIStudio,
                LLMProviderAPIKeys::GoogleAIStudio(GoogleAIStudioKey::new(
                    "AIzaSyCMkKfNkmjF8rTOWMg53NiYmz0Zv6xbfsE".to_owned(),
                )),
                request_id.to_owned(),
            );
            let result = self
                .tools
                .probing_results_summarize(request, request_id_ref)
                .await;
            let _ = self.ui_sender.send(UIEventWithID::probe_answer_event(
                request_id_ref.to_owned(),
                self.symbol_identifier.clone(),
                result
                    .as_ref()
                    .map(|s| s.to_owned())
                    .unwrap_or("Error with probing answer".to_owned()),
            ));
            println!(
                "Probing finished for {} with result: {:?}",
                &self.mecha_code_symbol.symbol_name(),
                &result
            );
            result
        }
    }

    /// Refreshing the state here implies the following:
    /// - we figure out all the implementations again and also
    /// our core snippet again
    /// - this way even if there have been changes we are almost always
    /// correct
    async fn refresh_state(&self, request_id: String) {
        self.mecha_code_symbol
            .refresh_state(self.ui_sender.clone(), &request_id)
            .await;
    }

    /// Sends additional requests to symbols which need changes or gathering more
    /// information to understand how to solve a problem
    ///
    /// TODO(codestory): This is disabled for now
    async fn _follow_along_requests(
        &self,
        request_id: &str,
        original_request: &str,
    ) -> Result<(), SymbolError> {
        if self.mecha_code_symbol.is_snippet_present().await {
            let symbol_content = self
                .mecha_code_symbol
                .get_symbol_content()
                .await
                .expect("snippet presence implies this would never fail");
            let symbols_to_follow = self
                .tools
                .follow_along_initial_query(
                    symbol_content,
                    original_request,
                    self.llm_properties.llm().clone(),
                    self.llm_properties.provider().clone(),
                    self.llm_properties.api_key().clone(),
                    request_id,
                )
                .await?;

            let mut pending_requests_to_hub: Vec<SymbolEventRequest> = vec![];

            // Now that we have the symbols to follow over here, we invoke a go-to-definition
            // for each of these symbols, if there are multiple matches we take the last one for
            // now
            for symbol_to_follow in symbols_to_follow.code_symbols_to_follow().into_iter() {
                let fs_file_path = symbol_to_follow.file_path().to_owned();
                let file_open_result = self
                    .tools
                    .file_open(fs_file_path.to_owned(), request_id)
                    .await?;
                let _ = self
                    .tools
                    .force_add_document(
                        &fs_file_path,
                        file_open_result.contents_ref(),
                        file_open_result.language(),
                    )
                    .await?;
                let symbol_to_follow_identifier = symbol_to_follow.symbol().to_owned();
                // Now we try to find the line in the file which matches with the line
                // content we are looking for and look for the symbol which contains it
                // one trick over here is that we are working within the scope of the current
                // symbol so we can filter out the content very easily and find the line we are
                // interested in by restricting the search space to be inside the outline node
                // content of the current symbol we are in
                let outline_nodes_maybe = self.tools.get_outline_nodes_grouped(&fs_file_path).await;
                if outline_nodes_maybe.is_none() {
                    continue;
                }
                let outline_nodes = outline_nodes_maybe.expect("is_none to hold above");
                // This should work for all identifiers and everything else
                // TODO(skcd): This logic does not work when we are looking at functions
                // inside the node
                let matching_outline_node = outline_nodes
                    .into_iter()
                    .find(|outline_node| outline_node.name() == self.symbol_name());
                if matching_outline_node.is_none() {
                    println!(
                        "symbol::follow_along_request::matching_outline_node::is_none({})::({})",
                        self.symbol_name(),
                        &symbol_to_follow_identifier,
                    );
                    continue;
                }
                let matching_outline_node = matching_outline_node.expect("is_none above to hold");
                let start_line = matching_outline_node.range().start_line();
                let line_containing_content = matching_outline_node
                    .content()
                    .content()
                    .lines()
                    .enumerate()
                    .map(|(idx, line)| (idx + start_line, line.to_string()))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .find(|(_, line)| line.contains(&symbol_to_follow.line_content()));
                if line_containing_content.is_none() {
                    println!(
                        "symbol::follow_along_request::line_containing_content::is_none({})::({})",
                        self.symbol_name(),
                        &symbol_to_follow_identifier,
                    );
                    continue;
                }
                let line_containing_content =
                    line_containing_content.expect("is_none to hold above");

                // Now we need to find the position of the needle in the line
                let column_position =
                    find_needle_position(&line_containing_content.1, symbol_to_follow.symbol());
                if column_position.is_none() {
                    println!(
                        "symbol::follow_along_request::find_needle_position::is_none({})::({})",
                        self.symbol_name(),
                        &symbol_to_follow_identifier,
                    );
                    continue;
                }
                let column_position = column_position.expect("is_none to hold above");

                // Now that we have the line and column, we execute a go-to-definition on this and grab the symbol
                // which it points to
                let mut definitions = self
                    .tools
                    .go_to_definition(
                        symbol_to_follow.file_path(),
                        Position::new(line_containing_content.0, column_position, 0),
                        request_id,
                    )
                    .await?
                    .definitions();

                // Now that we have the definition(s) for the symbol we can choose to send a request
                // to the symbol asking for a change request, we pick the first one over here
                // and then ask it the question along with the user query
                if definitions.is_empty() {
                    println!(
                        "symbol::follow_along_request::definition::is_empty({})::({})",
                        self.symbol_name(),
                        &symbol_to_follow_identifier,
                    );
                    continue;
                }
                let definition = definitions.remove(0);
                // check if definition belongs to one of the implementations we have
                // for the current symbol, that way its not really necessary to follow
                // along over here since we will handle it later on
                let implementations = self.mecha_code_symbol.get_implementations().await;
                if implementations.into_iter().any(|implementation| {
                    let implementation_file_path = implementation.file_path();
                    if definition.file_path() == implementation_file_path
                        && implementation
                            .range()
                            .contains_check_line(definition.range())
                    {
                        true
                    } else {
                        false
                    }
                }) {
                    println!(
                        "symbol::follow_along_request::self_follow_along_found({})::({})",
                        self.symbol_name(),
                        &symbol_to_follow_identifier,
                    );
                    continue;
                }
                let outline_node = self
                    .tools
                    .get_outline_node_for_range(
                        definition.range(),
                        definition.file_path(),
                        request_id,
                    )
                    .await?;

                let current_symbol_name = self.symbol_name();
                let thinking = symbol_to_follow.reason_for_selection();
                let request = format!(
                    r#"The user asked the following question:
{original_request}

We were initially at {current_symbol_name} and believe that to satisfy the user query we need to handle the following:

{thinking}

Satisfy the requirement either by making edits or gathering the required information so the user query can be handled at {current_symbol_name}"#
                );

                // Now that we have the outline node, we can create a request which we want to send
                // over to this symbol to handle
                // probing ???
                let symbol_identifier = SymbolIdentifier::with_file_path(
                    outline_node.name(),
                    outline_node.fs_file_path(),
                );

                // TODO(skcd): we want to avoid following ourselves, we should guard against
                // that over here
                if symbol_identifier != self.symbol_identifier {
                    pending_requests_to_hub.push(SymbolEventRequest::probe_request(
                        symbol_identifier.clone(),
                        SymbolToProbeRequest::new(
                            symbol_identifier,
                            request.to_owned(),
                            request,
                            request_id.to_owned(),
                            vec![],
                        ),
                        self.tool_properties.clone(),
                    ));
                }
            }

            let pending_futures = pending_requests_to_hub
                .into_iter()
                .map(|pending_request| {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let _ = self
                        .hub_sender
                        .send((pending_request, request_id.to_owned(), sender));
                    receiver
                })
                .collect::<Vec<_>>();

            let responses = stream::iter(pending_futures)
                .map(|pending_receiver| async move {
                    let response = pending_receiver.await;
                    if let Ok(response) = response {
                        Some(response.to_string())
                    } else {
                        None
                    }
                })
                .buffer_unordered(100)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .filter_map(|s| s)
                .collect::<Vec<_>>();
            println!("{:?}", &responses);
        }

        Ok(())
    }

    /// Initial request follows the following flow:
    /// - COT + follow-along questions for any other symbols which might even lead to edits
    /// - Reranking the snippets for the symbol
    /// - Edit the current symbol
    async fn generate_initial_request(
        &self,
        request_id: String,
        request_data: InitialRequestData,
        // TODO(codestory): This is a bit wrong, we will figure this out in due time
    ) -> Result<Option<SymbolEventRequest>, SymbolError> {
        println!(
            "symbol::generate_follow_along_requests::symbol_name({})",
            self.symbol_name()
        );
        // disable follow_along_requests so we can get to the code editing faster
        // let original_request = request_data.get_original_question();
        // let _ = self
        //     .follow_along_requests(&request_id, original_request)
        //     .await;
        if self.mecha_code_symbol.is_snippet_present().await {
            let request = if request_data.full_symbol_request() {
                self.mecha_code_symbol
                    .full_symbol_initial_request(
                        self.tools.clone(),
                        &request_data,
                        request_id,
                        &self.tool_properties,
                        self.hub_sender.clone(),
                        self.ui_sender.clone(),
                    )
                    .await?
            } else {
                Some(
                    self.mecha_code_symbol
                        .initial_request(
                            self.tools.clone(),
                            &request_data,
                            self.llm_properties.clone(),
                            request_id,
                            &self.tool_properties,
                            self.hub_sender.clone(),
                            self.ui_sender.clone(),
                        )
                        .await?,
                )
            };
            Ok(request)
        } else {
            let code_location_for_addition = self
                .tools
                .code_location_for_addition(
                    self.fs_file_path(),
                    &self.symbol_identifier,
                    request_data.get_original_question(),
                    &request_id,
                )
                .await?;
            let code_position = self
                .tools
                .add_empty_line_at_line(
                    self.fs_file_path(),
                    code_location_for_addition.0.line(),
                    code_location_for_addition.1,
                    &request_id,
                )
                .await?;

            // if the last line is not empty, then we want to create an empty line
            // and then start inserting the code over there
            let sub_symbol_to_edit = SymbolToEdit::new(
                self.symbol_name().to_owned(),
                Range::new(code_position.clone(), code_position),
                self.fs_file_path().to_owned(),
                vec![request_data.get_plan()],
                false,
                true,
                false,
                request_data.get_original_question().to_owned(),
                request_data
                    .symbols_edited_list()
                    .map(|symbol_edited_list| symbol_edited_list.to_vec()),
            );
            let mut history = request_data.history().to_vec();
            history.push(SymbolRequestHistoryItem::new(
                self.symbol_name().to_owned(),
                self.fs_file_path().to_owned(),
                request_data.get_original_question().to_owned(),
            ));
            // we need to do context gathering over here as well
            let context = self
                .grab_context_for_editing_faster(&sub_symbol_to_edit, &request_id)
                .await?;
            let _ = self
                .insert_code_full(&sub_symbol_to_edit, context, history, &request_id)
                .await;
            Ok(None)
        }
    }

    // The protocol here is that the questions are just plain text, its on the symbol
    // to decide if it needs to collect more information or make changes, we need to carefully
    // figure that out over here
    // what tools do we provide to the symbol for this?
    async fn _answer_question(&self, _question: &str) -> Result<SymbolEventRequest, SymbolError> {
        // The idea here we want to do is:
        // - We first ask which symbols we want to go towards and also do a global search
        // - We then do a question for any followup changes which we need to do on these other symbols (ask them a query and wait for the result)
        // - Use the returned data to create the final edit or answer question here as required
        // - Finally if our shape has changed we need to schedule followups
        todo!("we need to make sure this works")
    }

    /// Grabs the context which is required for editing, this is necessary to make
    /// the code editing work properly and use the right functions
    ///
    /// We look at the all symbols which are part of the import scope and filter
    /// them down and only include the outlines for it in the context, this way
    /// we reduce the amount of tokens and use a weaker model to generate the data
    pub async fn grab_context_for_editing_faster(
        &self,
        sub_symbol: &SymbolToEdit,
        request_id: &str,
    ) -> Result<Vec<String>, SymbolError> {
        self.tools
            .rerank_outline_nodes_for_code_editing(sub_symbol, request_id)
            .await
    }

    // TODO(skcd): Handle the cases where the outline is within a symbol and spread
    // across different lines (as is the case in typescript and python)
    // for now we are focussing on rust
    pub async fn grab_context_for_editing(
        &self,
        subsymbol: &SymbolToEdit,
        request_id: &str,
    ) -> Result<Vec<String>, SymbolError> {
        // force add the file again because the content might have changed
        let file_contents = self
            .tools
            .file_open(subsymbol.fs_file_path().to_owned(), request_id)
            .await?;
        let _ = self
            .tools
            .force_add_document(
                subsymbol.fs_file_path(),
                file_contents.contents_ref(),
                file_contents.language(),
            )
            .await;
        let file_content = self
            .tools
            .get_file_content(&subsymbol.fs_file_path())
            .await?;
        let symbol_to_edit = self
            .tools
            .find_sub_symbol_to_edit_with_name(self.symbol_name(), subsymbol, request_id)
            .await?;
        println!(
            "symbol::grab_context_for_editing::symbol_to_edit\n{:?}",
            &symbol_to_edit
        );
        let selection_range = symbol_to_edit.range();
        let _language = self
            .tools
            .detect_language(&subsymbol.fs_file_path())
            .unwrap_or("".to_owned());
        // we have 2 tools which can be used here and they are both kind of interesting:
        // - one of them is the grab definitions which are relevant
        // - one of them is the global context search
        // - first we try to check if the sub-symbol exists in the file
        let interested_defintiions = self
            .tools
            .gather_important_symbols_with_definition(
                symbol_to_edit.fs_file_path(),
                &file_content,
                selection_range,
                self.llm_properties.llm().clone(),
                self.llm_properties.provider().clone(),
                self.llm_properties.api_key().clone(),
                &subsymbol.instructions().join("\n"),
                self.hub_sender.clone(),
                request_id,
                &self.tool_properties,
            )
            .await?;

        // cool now we have all the symbols which are necessary for making the edit
        // and more importantly we have all the context which is required
        // we can send the edit request
        // this is the planning stage at this point, now we can begin the editing
        let outlines = interested_defintiions
            .iter()
            .filter_map(|interesed_definitions| {
                if let Some(interesed_definitions) = interesed_definitions {
                    Some(interesed_definitions.1.to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        Ok(outlines)
    }

    /// Helps find the sub-symbol location or creates a new location for it
    /// if we do not have any sub-symbol matching this one
    async fn find_sub_symbol_location(
        &self,
        mut sub_symbol: SymbolToEdit,
        request_id: &str,
    ) -> Result<SymbolToEdit, SymbolError> {
        self.refresh_state(request_id.to_owned()).await;
        // Grabs the implementation of the symbols over here
        let implementation_blocks = self.mecha_code_symbol.get_implementations().await;
        // in languages like typescript and python we have a single implementation block
        // of if its a new sub-symbol then we return it accordingly as the line just after
        // the last symbol
        // in case of rust, we have scoped implementation like: trait implementations, struct
        // and then impl blocks, for now we will find the first impl block which is not a trait
        // implementation and place the sub-symbol over there
        // we do need this data about the code symbol from the chunker
        // - we have to find the blocks of code which are implementations
        sub_symbol = self
            .tools
            .find_implementation_block_for_sub_symbol(sub_symbol, implementation_blocks.as_slice())
            .await?;
        Ok(sub_symbol)
    }

    async fn add_subsymbol(
        &self,
        sub_symbol: &SymbolToEdit,
        context: Vec<String>,
        request_id: &str,
    ) -> Result<EditedCodeSymbol, SymbolError> {
        println!(
            "symbol::add_subsymbol::sub_symbol_name({})",
            sub_symbol.symbol_name()
        );
        // update the sub-symbol location to the most appropriate place
        // TODO(codestory): Might need some debug logging on this section
        let range_to_insert = sub_symbol.range().clone();
        let content = "".to_owned();
        let (llm_properties, swe_bench_initial_edit) =
            if let Some(llm_properties) = self.tool_properties.get_swe_bench_code_editing_llm() {
                (llm_properties, true)
            } else {
                (self.llm_properties.clone(), false)
            };
        let file_content = self
            .tools
            .file_open(sub_symbol.fs_file_path().to_owned(), request_id)
            .await?;
        let response = self
            .tools
            .code_edit(
                sub_symbol.fs_file_path(),
                file_content.contents_ref(),
                &range_to_insert,
                context.join("\n"),
                sub_symbol.instructions().join("\n"),
                llm_properties.llm().clone(),
                llm_properties.provider().clone(),
                llm_properties.api_key().clone(),
                request_id,
                swe_bench_initial_edit,
                None,
                Some(sub_symbol.symbol_name().to_owned()),
                sub_symbol.symbol_edited_list(),
            )
            .await?;
        Ok(EditedCodeSymbol::new(content, response))
    }

    /// Editing the full symbol in total is a 2 step process
    /// - We use sonnet 3.5 for planning the changes and generating an outline
    /// of the changes
    /// - We then use a weaker model to apply the changes very quickly
    /// - Code correction happens naturally after that
    async fn edit_code_full(
        &self,
        // sub-symbol here referes to the full symbol not an individual symbol
        // inside the symbol itself
        sub_symbol: &SymbolToEdit,
        context: Vec<String>,
        request_id: &str,
    ) -> Result<EditedCodeSymbol, SymbolError> {
        let symbol_to_edit = self
            .tools
            .find_symbol_to_edit_closest_to_range(sub_symbol, request_id)
            .await?;
        let content = symbol_to_edit.content().to_owned();
        let file_content = self
            .tools
            .file_open(sub_symbol.fs_file_path().to_owned(), request_id)
            .await?;

        // check if we should continue editing here
        println!(
            "symbol::edit_code_full::should_edit::start({})",
            self.symbol_name()
        );

        // Here there are 2 steps which will need to happen:
        // - First we use a more powerful model for generating the outline of the changes
        // which need to be done
        // - Second: we use a weaker model to generate the final outline of the changes which will be done
        let edited_code = self
            .tools
            .code_edit_outline(
                sub_symbol,
                &self.symbol_identifier,
                sub_symbol.fs_file_path(),
                file_content.contents_ref(),
                symbol_to_edit.range(),
                context.join("\n"),
                sub_symbol.instructions().join("\n"),
                self.llm_properties.clone(),
                request_id,
                Some(symbol_to_edit.name().to_owned()),
                sub_symbol.symbol_edited_list(),
            )
            .await?;

        Ok(EditedCodeSymbol::new(content, edited_code))
    }

    /// Inserts new code to the file, this is useful when the symbol is not present
    /// in the file and needs to be inserted anew
    async fn insert_code_full(
        &self,
        sub_symbol: &SymbolToEdit,
        context: Vec<String>,
        history: Vec<SymbolRequestHistoryItem>,
        request_id: &str,
    ) -> Result<(), SymbolError> {
        println!(
            "symbol::insert_code_full::start::symbol({})",
            self.symbol_name()
        );
        let file_content = self
            .tools
            .file_open(sub_symbol.fs_file_path().to_owned(), request_id)
            .await?;
        // always return the original code which was present here in case of rollbacks
        let _ = self.ui_sender.send(UIEventWithID::range_selection_for_edit(
            request_id.to_owned(),
            self.symbol_identifier.clone(),
            sub_symbol.range().clone(),
            sub_symbol.fs_file_path().to_owned(),
        ));
        println!(
            "symbol::insert_code_full::code_edit_outline::symbol({})",
            self.symbol_name()
        );
        let edited_code = self
            .tools
            .code_edit_outline(
                sub_symbol,
                // forcefully send the symbol identifier because we depend on the
                // fs_file_path being non-null on the editor side
                &SymbolIdentifier::with_file_path(self.symbol_name(), sub_symbol.fs_file_path()),
                sub_symbol.fs_file_path(),
                file_content.contents_ref(),
                sub_symbol.range(),
                context.join("\n"),
                sub_symbol.instructions().join("\n"),
                self.llm_properties.clone(),
                request_id,
                Some(sub_symbol.symbol_name().to_owned()),
                sub_symbol.symbol_edited_list(),
            )
            .await?;
        let _ = self.ui_sender.send(UIEventWithID::edited_code(
            request_id.to_owned(),
            self.symbol_identifier.clone(),
            // we need to reshape our range here for the edited
            // code so we just make sure that the end line is properly
            // moved
            sub_symbol
                .range()
                .clone()
                .reshape_for_selection(&edited_code),
            sub_symbol.fs_file_path().to_owned(),
            edited_code.to_owned(),
        ));

        // TODO(skcd): We also want to generate followups over here after applying
        // the edits, that can come later on tho
        println!(
            "symbol::insert_code_full::check_code_correctness::symbol({})",
            self.symbol_name()
        );
        let context_str = context.join("\n");
        let _ = self
            .tools
            .check_code_correctness(
                self.symbol_name(),
                sub_symbol,
                self.symbol_identifier.clone(),
                "",
                &edited_code,
                &context_str,
                self.llm_properties.llm().clone(),
                self.llm_properties.provider().clone(),
                self.llm_properties.api_key().clone(),
                request_id,
                &self.tool_properties,
                history,
                self.hub_sender.clone(),
            )
            .await?;
        Ok(())
    }

    async fn edit_code(
        &self,
        sub_symbol: &SymbolToEdit,
        context: Vec<String>,
        request_id: &str,
    ) -> Result<EditedCodeSymbol, SymbolError> {
        // TODO(skcd): This part of the lookup is not correct because we might
        // want to edit the private members of the class and its an outline
        // instead if we pass the outline over here we get back the whole symbol
        // instead
        let symbol_to_edit = self
            .tools
            .find_sub_symbol_to_edit_with_name(self.symbol_name(), sub_symbol, request_id)
            .await?;
        let content = symbol_to_edit.content().to_owned();
        let (llm_properties, swe_bench_initial_edit) =
            if let Some(llm_properties) = self.tool_properties.get_swe_bench_code_editing_llm() {
                // if the symbol is extremely long which we want to edit, fallback
                // to an llm with a bigger context window, in this case we use gpt4-32k
                if symbol_to_edit.range().line_size() > 500 {
                    if let Some(llm_properties_long_context) =
                        self.tool_properties.get_long_context_editing_llm()
                    {
                        (llm_properties_long_context, true)
                    } else {
                        (llm_properties, true)
                    }
                } else {
                    (llm_properties, true)
                }
            } else {
                (self.llm_properties.clone(), false)
            };
        let file_content = self
            .tools
            .file_open(sub_symbol.fs_file_path().to_owned(), request_id)
            .await?;
        let response = self
            .tools
            .code_edit(
                sub_symbol.fs_file_path(),
                file_content.contents_ref(),
                symbol_to_edit.range(),
                context.join("\n"),
                sub_symbol.instructions().join("\n"),
                llm_properties.llm().clone(),
                llm_properties.provider().clone(),
                llm_properties.api_key().clone(),
                request_id,
                swe_bench_initial_edit,
                Some(symbol_to_edit.name().to_owned()),
                None,
                sub_symbol.symbol_edited_list(),
            )
            .await?;
        Ok(EditedCodeSymbol::new(content, response))
    }

    // we are going to edit the symbols over here
    // some challenges:
    // - we want this to be fully parallel first of all
    // - we also have to do the follow-up fixes on this when we are done editing
    // - we have to look at the lsp information as well
    // - we also want it to be fully parallel
    async fn edit_implementations(
        &self,
        edit_request: SymbolToEditRequest,
        request_id: String,
    ) -> Result<(), SymbolError> {
        let request_id_ref = &request_id;

        // NOTE: we do not add an entry to the history here because the initial
        // request already adds the entry before sending over the edit
        let history = edit_request.history().to_vec();
        // here we might want to edit ourselves or generate new code depending
        // on the scope of the changes being made
        let sub_symbols_to_edit = edit_request.symbols();
        println!(
            "symbol::edit_implementations::sub_symbols::({}).len({})",
            self.symbol_name(),
            sub_symbols_to_edit.len()
        );
        // edit requires the following:
        // - gathering context for the symbols which the definitions or outlines are required
        // - do a COT to figure out how to go about making the changes
        // - making the edits
        // - following the changed symbol to check on the references and wherever its being used
        for mut sub_symbol_to_edit in sub_symbols_to_edit.into_iter() {
            println!(
                "symbol::edit_implementation::sub_symbol_to_edit::({})::is_new({:?})",
                sub_symbol_to_edit.symbol_name(),
                sub_symbol_to_edit.is_new(),
            );

            let context_for_editing = if sub_symbol_to_edit.is_new() {
                self.grab_context_for_editing_faster(&sub_symbol_to_edit, &request_id_ref)
                    .await?
            } else {
                if sub_symbol_to_edit.is_full_edit() {
                    self.grab_context_for_editing_faster(&sub_symbol_to_edit, &request_id_ref)
                        .await?
                } else {
                    self.grab_context_for_editing(&sub_symbol_to_edit, request_id_ref)
                        .await?
                }
            };

            // if this is a new sub-symbol we have to create we have to diverge the
            // implementations a bit or figure out how to edit with a new line added
            // to the end of the symbol
            let edited_code = if sub_symbol_to_edit.is_new() {
                let _ = self.ui_sender.send(UIEventWithID::range_selection_for_edit(
                    request_id_ref.to_owned(),
                    self.symbol_identifier.clone(),
                    sub_symbol_to_edit.range().to_owned(),
                    sub_symbol_to_edit.fs_file_path().to_owned(),
                ));
                println!(
                    "symbol::edit_implementation::add_subsymbol::({})::({})",
                    self.symbol_name(),
                    sub_symbol_to_edit.symbol_name()
                );
                // Find the location for the sub-symbol
                sub_symbol_to_edit = self
                    .find_sub_symbol_location(sub_symbol_to_edit.clone(), request_id_ref)
                    .await?;
                self.add_subsymbol(
                    &sub_symbol_to_edit,
                    context_for_editing.to_owned(),
                    &request_id,
                )
                .await?
            } else {
                // always return the original code which was present here in case of rollbacks
                let _ = self.ui_sender.send(UIEventWithID::range_selection_for_edit(
                    request_id_ref.to_owned(),
                    self.symbol_identifier.clone(),
                    sub_symbol_to_edit.range().clone(),
                    sub_symbol_to_edit.fs_file_path().to_owned(),
                ));

                // Toggle between the full edit mode and the accurate edits
                if sub_symbol_to_edit.is_full_edit() {
                    self.edit_code_full(
                        &sub_symbol_to_edit,
                        context_for_editing.to_vec(),
                        &request_id,
                    )
                    .await?
                } else {
                    self.edit_code(
                        &sub_symbol_to_edit,
                        context_for_editing.to_vec(),
                        &request_id,
                    )
                    .await?
                }
            };
            let original_code = &edited_code.original_code;
            let edited_code = &edited_code.edited_code;
            // send over edited code request
            let _ = self.ui_sender.send(UIEventWithID::edited_code(
                request_id.to_owned(),
                self.symbol_identifier.clone(),
                // we need to reshape our range here for the edited
                // code so we just make sure that the end line is properly
                // moved
                sub_symbol_to_edit
                    .range()
                    .clone()
                    .reshape_for_selection(&edited_code),
                sub_symbol_to_edit.fs_file_path().to_owned(),
                edited_code.to_owned(),
            ));
            println!(
                "symbol::edit_implementation::check_code_correctness::({})",
                self.symbol_name()
            );
            // debugging loop after this
            let _ = self
                .tools
                .check_code_correctness(
                    self.symbol_name(),
                    &sub_symbol_to_edit,
                    self.symbol_identifier.clone(),
                    original_code,
                    edited_code,
                    &context_for_editing.join("\n"),
                    self.llm_properties.llm().clone(),
                    self.llm_properties.provider().clone(),
                    self.llm_properties.api_key().clone(),
                    request_id_ref,
                    &self.tool_properties,
                    history.to_vec(),
                    self.hub_sender.clone(),
                )
                .await;

            // once we have successfully changed the implementation over here
            // we have to start looking for followups over here
            // F in the chat for error handling :')
            let _ = self
                .tools
                .check_for_followups(
                    self.symbol_name(),
                    &sub_symbol_to_edit,
                    &original_code,
                    self.llm_properties.llm().clone(),
                    self.llm_properties.provider().clone(),
                    self.llm_properties.api_key().clone(),
                    self.hub_sender.clone(),
                    request_id_ref,
                    &self.tool_properties,
                )
                .await;
        }
        println!(
            "symbol::edit_implementation::finish::({})",
            self.symbol_name()
        );
        Ok(())
    }

    // this is the function which is polling the requests from the hub
    // we also want another loop which allows the agent to wait
    // for the requests which it was waiting for after sending it to the hub
    pub async fn run(
        self,
        receiver: UnboundedReceiver<(
            SymbolEvent,
            String,
            // we had a single sender over here as a future we can poll
            // for to receieve events from
            tokio::sync::oneshot::Sender<SymbolEventResponse>,
        )>,
    ) -> Result<(), SymbolError> {
        println!("Symbol::run({}) at types.rs", self.symbol_name());
        let receiver_stream = UnboundedReceiverStream::new(receiver);
        receiver_stream
            .map(|symbol_event| (symbol_event, self.clone()))
            .map(|(symbol_event, symbol)| async move {
                let (event, request_id, sender) = symbol_event;
                println!(
                    "Symbol::receiver_stream::event::({})\n{:?}",
                    symbol.symbol_name(),
                    &event
                );
                let _ = symbol.ui_sender.send(UIEventWithID::from_symbol_event(
                    request_id.to_owned(),
                    SymbolEventRequest::new(
                        symbol.symbol_identifier.clone(),
                        event.clone(),
                        symbol.tool_properties.clone(),
                    ),
                ));
                match event {
                    SymbolEvent::InitialRequest(initial_request) => {
                        println!("Symbol::inital_request: {}", symbol.symbol_name());
                        symbol.refresh_state(request_id.to_owned()).await;
                        let initial_request = symbol
                            .generate_initial_request(request_id.to_owned(), initial_request)
                            .await;
                        let request_sender = sender;
                        println!(
                            "Symbol::initial_request::generated({}).is_ok({})",
                            symbol.symbol_name(),
                            initial_request.is_ok(),
                        );
                        match initial_request {
                            Ok(Some(initial_request)) => {
                                let (sender, receiver) = tokio::sync::oneshot::channel();
                                let _ = symbol.hub_sender.send((
                                    initial_request.clone(),
                                    // since this is the initial request, we will end up generating
                                    // a new request id for this
                                    uuid::Uuid::new_v4().to_string(),
                                    sender,
                                ));
                                let response = receiver.await;
                                if response.is_err() {
                                    println!(
                                        "symbol.hub_sender::({})::initial_request({:?})",
                                        symbol.symbol_name(),
                                        &initial_request,
                                    );
                                }
                                println!(
                                    "Response from symbol.hub_sender::({}): {:?}",
                                    symbol.symbol_name(),
                                    &response,
                                );
                                // ideally we want to give this resopnse back to the symbol
                                // so it can keep track of everything that its doing, we will get to that
                                let _ = request_sender.send(SymbolEventResponse::TaskDone(
                                    "initial request done".to_owned(),
                                ));
                                Ok(())
                            }
                            Ok(None) => {
                                println!("symbol::run::initial_request::empy_response");
                                let _ = request_sender.send(SymbolEventResponse::TaskDone(
                                    "initial request done".to_owned(),
                                ));
                                Ok(())
                            }
                            Err(e) => {
                                println!("symbol::run::initial_request::({:?})", &e);
                                let _ = request_sender.send(SymbolEventResponse::TaskDone(
                                    "initial request done".to_owned(),
                                ));
                                Err(e)
                            }
                        }
                    }
                    SymbolEvent::Edit(edit_request) => {
                        // we refresh our state always
                        println!(
                            "symbol::types::symbol_event::edit::refresh_state({})",
                            symbol.symbol_name()
                        );
                        symbol.refresh_state(request_id.to_owned()).await;
                        // one of the primary goals here is that we can make edits
                        // everywhere at the same time unless its on the same file
                        // but for now, we are gonna pleb our way and make edits
                        // one by one
                        println!(
                            "symbol::types::symbol_event::edit::edit_implementations({})",
                            symbol.symbol_name()
                        );
                        let _ = symbol.edit_implementations(edit_request, request_id).await;
                        let _ =
                            sender.send(SymbolEventResponse::TaskDone("edit finished".to_owned()));
                        Ok(())
                    }
                    SymbolEvent::AskQuestion(_ask_question_request) => {
                        // we refresh our state always
                        symbol.refresh_state(request_id).await;
                        // we will the following in sequence:
                        // - ask for information from surrounding nodes
                        // - refresh the state
                        // - ask for changes which need to be made to the surrounding nodes
                        // - refresh our state
                        // - edit ourselves if required or formulate the answer
                        // - followup
                        // - task 1: sending probes to the world about gathering information
                        todo!("ask question is not implemented yet");
                    }
                    SymbolEvent::Delete => {
                        todo!("delete is not implemented yet");
                    }
                    SymbolEvent::UserFeedback => {
                        todo!("user feedback is not implemented yet");
                    }
                    SymbolEvent::Outline => {
                        // we have been asked to provide an outline of the symbol we are part of
                        // this is a bit easy to do so lets try and finish this
                        let outline = symbol.get_outline(request_id).await;
                        let _ = match outline {
                            Ok(outline) => sender.send(SymbolEventResponse::TaskDone(outline)),
                            Err(_) => sender.send(SymbolEventResponse::TaskDone(
                                "failed to get outline".to_owned(),
                            )),
                        };
                        Ok(())
                    }
                    SymbolEvent::Probe(probe_request) => {
                        // we make the probe request an explicit request
                        // we are still going to do the same things just
                        // that this one is for gathering answeres
                        let reply = symbol
                            .probe_request_handler(
                                probe_request,
                                symbol.hub_sender.clone(),
                                request_id.to_owned(),
                            )
                            .await;
                        let _ = match reply {
                            Ok(reply) => {
                                let _ = symbol.ui_sender.send(UIEventWithID::sub_symbol_step(
                                    request_id,
                                    SymbolEventSubStepRequest::probe_answer(
                                        symbol.symbol_identifier.clone(),
                                        reply.to_owned(),
                                    ),
                                ));
                                let _ = sender.send(SymbolEventResponse::TaskDone(reply));
                            }
                            Err(e) => {
                                println!("Error when probing: {:?}", e);
                                let _ = symbol.ui_sender.send(UIEventWithID::sub_symbol_step(
                                    request_id,
                                    SymbolEventSubStepRequest::probe_answer(
                                        symbol.symbol_identifier.clone(),
                                        "failed to answer the user query because of external error"
                                            .to_owned(),
                                    ),
                                ));
                                let _ = sender.send(SymbolEventResponse::TaskDone(
                                    "failed to look depeer to answer user query".to_owned(),
                                ));
                            }
                        };
                        Ok(())
                    }
                }
            })
            .buffer_unordered(1000)
            .collect::<Vec<_>>()
            .await;
        Ok(())
    }
}
