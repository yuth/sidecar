//! Creates the service which handles saving the session and extending it

use std::{collections::HashMap, sync::Arc};

use llm_client::broker::LLMBroker;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::{
    agentic::{
        symbol::{
            errors::SymbolError,
            events::{edit::SymbolToEdit, message_event::SymbolEventMessageProperties},
            identifier::SymbolIdentifier,
            manager::SymbolManager,
            scratch_pad::ScratchPadAgent,
            tool_box::ToolBox,
            ui_event::UIEventWithID,
        },
        tool::{
            broker::ToolBroker,
            helpers::diff_recent_changes::DiffFileContent,
            input::{ToolInput, ToolInputPartial},
            lsp::{
                file_diagnostics::DiagnosticMap, open_file::OpenFileRequest,
                search_file::SearchFileContentInput,
            },
            plan::service::PlanService,
            r#type::{Tool, ToolType},
            repo_map::generator::RepoMapGeneratorRequest,
            session::{session::AgentToolUseOutput, tool_use_agent::ToolUseAgent},
            terminal::terminal::TerminalInput,
        },
    },
    chunking::text_document::{Position, Range},
    repo::types::RepoRef,
    user_context::types::UserContext,
};

use super::session::{AideAgentMode, Session};

/// The session service which takes care of creating the session and manages the storage
pub struct SessionService {
    tool_box: Arc<ToolBox>,
    symbol_manager: Arc<SymbolManager>,
    running_exchanges: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl SessionService {
    pub fn new(tool_box: Arc<ToolBox>, symbol_manager: Arc<SymbolManager>) -> Self {
        Self {
            tool_box,
            symbol_manager,
            running_exchanges: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn track_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        cancellation_token: CancellationToken,
    ) {
        let hash_id = format!("{}-{}", session_id, exchange_id);
        let mut running_exchanges = self.running_exchanges.lock().await;
        running_exchanges.insert(hash_id, cancellation_token);
    }

    pub async fn get_cancellation_token(
        &self,
        session_id: &str,
        exchange_id: &str,
    ) -> Option<CancellationToken> {
        let hash_id = format!("{}-{}", session_id, exchange_id);
        let running_exchanges = self.running_exchanges.lock().await;
        running_exchanges
            .get(&hash_id)
            .map(|cancellation_token| cancellation_token.clone())
    }

    pub fn create_new_session_with_tools(
        &self,
        session_id: &str,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        storage_path: String,
        tools: Vec<ToolType>,
    ) -> Session {
        Session::new(
            session_id.to_owned(),
            project_labels,
            repo_ref,
            storage_path,
            UserContext::default(),
            tools,
        )
    }

    fn create_new_session(
        &self,
        session_id: String,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        storage_path: String,
        global_user_context: UserContext,
    ) -> Session {
        Session::new(
            session_id,
            project_labels,
            repo_ref,
            storage_path,
            global_user_context,
            vec![],
        )
    }

    pub async fn human_message(
        &self,
        session_id: String,
        storage_path: String,
        exchange_id: String,
        human_message: String,
        user_context: UserContext,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        agent_mode: AideAgentMode,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        println!("session_service::human_message::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session(
                session_id.to_owned(),
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                user_context.clone(),
            )
        };

        println!("session_service::session_created");

        // add human message
        session = session.human_message(
            exchange_id.to_owned(),
            human_message,
            user_context,
            project_labels,
            repo_ref,
        );

        let plan_exchange_id = self
            .tool_box
            .create_new_exchange(session_id.to_owned(), message_properties.clone())
            .await?;

        let cancellation_token = tokio_util::sync::CancellationToken::new();
        self.track_exchange(&session_id, &plan_exchange_id, cancellation_token.clone())
            .await;
        message_properties = message_properties
            .set_request_id(plan_exchange_id)
            .set_cancellation_token(cancellation_token);

        // now react to the last message
        session = session
            .reply_to_last_exchange(
                agent_mode,
                self.tool_box.clone(),
                exchange_id,
                message_properties,
            )
            .await?;

        // save the session to the disk
        self.save_to_storage(&session).await?;
        Ok(())
    }

    /// Takes the user iteration request and regenerates the plan a new
    /// by reacting according to the user request
    pub async fn plan_iteration(
        &self,
        session_id: String,
        storage_path: String,
        plan_storage_path: String,
        plan_id: String,
        plan_service: PlanService,
        exchange_id: String,
        iteration_request: String,
        user_context: UserContext,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        _root_directory: String,
        _codebase_search: bool,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        // Things to figure out:
        // - should we rollback all the changes we did before over here or build
        // on top of it
        // - we have to send the messages again on the same request over here
        // which implies that the same exchange id will be used to reset the plan which
        // has already happened
        // - we need to also send an event stating that the review pane needs a refresh
        // since we are generating a new request over here
        println!("session_service::plan::plan_iteration::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session(
                session_id.to_owned(),
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                user_context.clone(),
            )
        };
        // One trick over here which we can do for now is keep track of the
        // exchange which we are going to reply to this way we make sure
        // that we are able to get the right exchange properly
        let user_plan_request_exchange = session.get_parent_exchange_id(&exchange_id);
        if let None = user_plan_request_exchange {
            return Ok(());
        }
        let user_plan_request_exchange = user_plan_request_exchange.expect("if let None to hold");
        let user_plan_exchange_id = user_plan_request_exchange.exchange_id().to_owned();
        session = session.plan_iteration(
            user_plan_request_exchange.exchange_id().to_owned(),
            iteration_request.to_owned(),
            user_context,
        );
        // send a chat message over here telling the editor about the followup:
        let _ = message_properties
            .ui_sender()
            .send(UIEventWithID::chat_event(
                session_id.to_owned(),
                user_plan_exchange_id.to_owned(),
                "".to_owned(),
                Some(format!(
                    r#"\n### Followup:
{iteration_request}"#
                )),
            ));

        let user_plan_request_exchange =
            session.get_exchange_by_id(user_plan_request_exchange.exchange_id());
        self.save_to_storage(&session).await?;
        // we get the exchange using the parent id over here, since what we get
        // here is the reply_exchange and we want to get the parent one to which we
        // are replying since thats the source of truth
        // keep track of the user requests for the plan generation as well since
        // we are iterating quite a bit
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        message_properties = message_properties
            .set_request_id(exchange_id.to_owned())
            .set_cancellation_token(cancellation_token);
        // now we can perform the plan generation over here
        session = session
            .perform_plan_generation(
                plan_service,
                plan_id,
                user_plan_exchange_id,
                user_plan_request_exchange,
                plan_storage_path,
                self.tool_box.clone(),
                self.symbol_manager.clone(),
                message_properties,
            )
            .await?;
        // save the session to the disk
        self.save_to_storage(&session).await?;

        println!("session_service::plan_iteration::stop");
        Ok(())
    }

    /// Generates the plan over here and upon invocation we take care of executing
    /// the steps
    pub async fn plan_generation(
        &self,
        session_id: String,
        storage_path: String,
        plan_storage_path: String,
        plan_id: String,
        plan_service: PlanService,
        exchange_id: String,
        query: String,
        user_context: UserContext,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        _root_directory: String,
        _codebase_search: bool,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        println!("session_service::plan::agentic::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session(
                session_id.to_owned(),
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                user_context.clone(),
            )
        };

        // add an exchange that we are going to genrate a plan over here
        session = session.plan(exchange_id.to_owned(), query, user_context);
        self.save_to_storage(&session).await?;

        let exchange_in_focus = session.get_exchange_by_id(&exchange_id);

        // create a new exchange over here for the plan
        let plan_exchange_id = self
            .tool_box
            .create_new_exchange(session_id.to_owned(), message_properties.clone())
            .await?;
        println!("session_service::plan_generation::create_new_exchange::session_id({})::plan_exchange_id({})", &session_id, &plan_exchange_id);

        let cancellation_token = tokio_util::sync::CancellationToken::new();
        self.track_exchange(&session_id, &plan_exchange_id, cancellation_token.clone())
            .await;
        message_properties = message_properties
            .set_request_id(plan_exchange_id)
            .set_cancellation_token(cancellation_token);
        // now we can perform the plan generation over here
        session = session
            .perform_plan_generation(
                plan_service,
                plan_id,
                exchange_id.to_owned(),
                exchange_in_focus,
                plan_storage_path,
                self.tool_box.clone(),
                self.symbol_manager.clone(),
                message_properties,
            )
            .await?;
        // save the session to the disk
        self.save_to_storage(&session).await?;

        println!("session_service::plan_generation::stop");
        Ok(())
    }

    /// TODO(skcd): Pick up the integration from here for the tool use
    pub async fn tool_use_agentic(
        &self,
        session_id: String,
        storage_path: String,
        user_message: String,
        exchange_id: String,
        all_files: Vec<String>,
        open_files: Vec<String>,
        shell: String,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        root_directory: String,
        tool_box: Arc<ToolBox>,
        tool_broker: Arc<ToolBroker>,
        llm_broker: Arc<LLMBroker>,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        println!("session_service::tool_use_agentic::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session_with_tools(
                &session_id,
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                vec![
                    ToolType::ListFiles,
                    ToolType::SearchFileContentWithRegex,
                    ToolType::OpenFile,
                    ToolType::CodeEditing,
                    ToolType::LSPDiagnostics,
                    // disable for testing
                    // ToolType::AskFollowupQuestions,
                    ToolType::AttemptCompletion,
                    ToolType::RepoMapGeneration,
                    ToolType::TerminalCommand,
                ],
            )
        };

        // os can be passed over here safely since we can assume the sidecar is running
        // close to the vscode server
        // we should ideally get this information from the vscode-server side setting
        let tool_agent = ToolUseAgent::new(
            llm_broker.clone(),
            root_directory,
            std::env::consts::OS.to_owned(),
            shell.to_owned(),
        );

        session = session.human_message_tool_use(
            exchange_id.to_owned(),
            user_message,
            all_files,
            open_files,
            shell,
        );
        let _ = self.save_to_storage(&session).await;

        session = session.accept_open_exchanges_if_any(message_properties.clone());
        let mut human_message_ticker = 0;
        // now that we have saved it we can start the loop over here and look out for the cancellation
        // token which will imply that we should end the current loop
        loop {
            let _ = self.save_to_storage(&session).await;
            let tool_exchange_id = self
                .tool_box
                .create_new_exchange(session_id.to_owned(), message_properties.clone())
                .await?;

            let cancellation_token = tokio_util::sync::CancellationToken::new();

            message_properties = message_properties
                .set_request_id(tool_exchange_id.to_owned())
                .set_cancellation_token(cancellation_token.clone());

            // track the new exchange over here
            self.track_exchange(&session_id, &tool_exchange_id, cancellation_token.clone())
                .await;

            let tool_use_output = dbg!(
                session
                    // the clone here is pretty bad but its the easiest and the sanest
                    // way to keep things on the happy path
                    .clone()
                    .get_tool_to_use(
                        tool_box.clone(),
                        tool_exchange_id,
                        exchange_id.to_owned(),
                        tool_agent.clone(),
                        message_properties.clone(),
                    )
                    .await
            )?;

            match tool_use_output {
                AgentToolUseOutput::Success((tool_input_partial, new_session)) => {
                    // update our session
                    session = new_session;
                    // store to disk
                    let _ = self.save_to_storage(&session).await;
                    // execute the partial tool input and get the final output here
                    match tool_input_partial {
                        ToolInputPartial::AskFollowupQuestions(followup_question) => {
                            println!("Ask followup question: {}", followup_question.question());
                            let input = ToolInput::AskFollowupQuestions(followup_question);
                            let response = tool_broker.invoke(input).await;
                            println!("response: {:?}", response);
                        }
                        ToolInputPartial::AttemptCompletion(attempt_completion) => {
                            println!("LLM reached a stop condition");
                            println!("{:?}", &attempt_completion);
                            break;
                        }
                        ToolInputPartial::CodeEditing(code_editing) => {
                            let fs_file_path = code_editing.fs_file_path().to_owned();
                            println!("Code editing: {}", fs_file_path);
                            let file_contents = tool_box
                                .file_open(fs_file_path.to_owned(), message_properties.clone())
                                .await
                                .expect("file_contents to work")
                                .contents();

                            let instruction = code_editing.instruction().to_owned();

                            // keep track of the file content which we are about to modify over here
                            let old_file_content = self
                                .tool_box
                                .file_open(fs_file_path.to_owned(), message_properties.clone())
                                .await;

                            let default_range =
                            // very large end position
                                Range::new(Position::new(0, 0, 0), Position::new(10_000, 0, 0));

                            let symbol_to_edit = SymbolToEdit::new(
                                fs_file_path.to_owned(),
                                default_range,
                                fs_file_path.to_owned(),
                                vec![instruction.clone()],
                                false,
                                false, // is_new
                                false,
                                "".to_owned(),
                                None,
                                false,
                                None,
                                false,
                                None,
                                vec![], // previous_user_queries
                                None,
                            );

                            let symbol_identifier = SymbolIdentifier::new_symbol(&fs_file_path);

                            let response = tool_box
                                .code_editing_with_search_and_replace(
                                    &symbol_to_edit,
                                    &fs_file_path,
                                    &file_contents,
                                    &default_range,
                                    "".to_owned(),
                                    instruction.clone(),
                                    &symbol_identifier,
                                    None,
                                    None,
                                    message_properties.clone(),
                                )
                                .await
                                .expect("to work"); // big expectations but can also fail, we should handle it properly

                            // now that we have modified the file we can ask the editor for the git-diff of this file over here
                            // and we also have the previous state over here
                            let diff_changes = self
                                .tool_box
                                .recently_edited_files_with_content(
                                    vec![fs_file_path.to_owned()].into_iter().collect(),
                                    match old_file_content {
                                        Ok(old_file_content) => {
                                            vec![DiffFileContent::new(
                                                fs_file_path.to_owned(),
                                                old_file_content.contents(),
                                            )]
                                        }
                                        Err(_) => vec![],
                                    },
                                    message_properties.clone(),
                                )
                                .await?;

                            // we need to take the L1 level changes here since those are the ones we are interested in and then add
                            // that as a human message over here
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                format!(r#"I performed the edits which you asked for, here is the git diff for it:
{}"#, diff_changes.l1_changes()),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", response);
                        }
                        ToolInputPartial::LSPDiagnostics(diagnostics) => {
                            println!("LSP diagnostics: {:?}", diagnostics);
                            // figure out what do to with this, we should probably just gather all the diagnostics
                            // and pass it along as a user message
                            let diagnostics_output = dbg!(
                                tool_box
                                    .grab_workspace_diagnostics(message_properties.clone())
                                    .await
                            )
                            .expect("big expectation for diagnostics to never fail");
                            let diagnostics_grouped_by_file: DiagnosticMap = diagnostics_output
                                .0
                                .into_iter()
                                .fold(HashMap::new(), |mut acc, error| {
                                    acc.entry(error.fs_file_path().to_owned())
                                        .or_insert_with(Vec::new)
                                        .push(error);
                                    acc
                                });

                            let formatted_diagnostics =
                                PlanService::format_diagnostics(&diagnostics_grouped_by_file);
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                formatted_diagnostics,
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                        }
                        ToolInputPartial::ListFiles(list_files) => {
                            println!("list files: {}", list_files.directory_path());
                            let input = ToolInput::ListFiles(list_files);
                            let response = tool_broker.invoke(input).await;
                            let list_files_output = response
                                .expect("to work")
                                .get_list_files_directory()
                                .expect("to work");
                            let response = list_files_output
                                .files()
                                .into_iter()
                                .map(|file_path| file_path.to_string_lossy().to_string())
                                .collect::<Vec<_>>()
                                .join("\n");
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                response.to_owned(),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", response);
                        }
                        ToolInputPartial::OpenFile(open_file) => {
                            println!("open file: {}", open_file.fs_file_path());
                            let open_file_path = open_file.fs_file_path().to_owned();
                            let request = OpenFileRequest::new(
                                open_file_path,
                                message_properties.editor_url(),
                            );
                            let input = ToolInput::OpenFile(request);
                            let response = tool_broker
                                .invoke(input)
                                .await
                                .expect("to work")
                                .get_file_open_response()
                                .expect("to work")
                                .to_string();
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                response.clone(),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", response);
                        }
                        ToolInputPartial::SearchFileContentWithRegex(search_file) => {
                            println!("search file: {}", search_file.directory_path());
                            let request = SearchFileContentInput::new(
                                search_file.directory_path().to_owned(),
                                search_file.regex_pattern().to_owned(),
                                search_file.file_pattern().map(|s| s.to_owned()),
                                message_properties.editor_url(),
                            );
                            let input = ToolInput::SearchFileContentWithRegex(request);
                            let tool_response = tool_broker.invoke(input).await.expect("to work");
                            let response = tool_response
                                .get_search_file_content_with_regex()
                                .expect("to work");
                            let response = response.response();
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                response.to_owned(),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", response);
                        }
                        ToolInputPartial::TerminalCommand(terminal_command) => {
                            println!("terminal command: {}", terminal_command.command());
                            let command = terminal_command.command().to_owned();
                            let request =
                                TerminalInput::new(command, message_properties.editor_url());
                            let input = ToolInput::TerminalCommand(request);
                            let tool_output = tool_broker.invoke(input).await;
                            let output = tool_output
                                .expect("to work")
                                .terminal_command()
                                .expect("to work")
                                .output()
                                .to_owned();
                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                output.to_owned(),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", output);
                        }
                        ToolInputPartial::RepoMapGeneration(repo_map_request) => {
                            println!(
                                "repo map generation request: {}",
                                repo_map_request.to_string()
                            );
                            let request =
                                ToolInput::RepoMapGeneration(RepoMapGeneratorRequest::new(
                                    repo_map_request.directory_path().to_owned(),
                                    3000,
                                ));
                            let tool_output = tool_broker.invoke(request).await;
                            let repo_map_str = tool_output
                                .expect("to work")
                                .repo_map_generator_response()
                                .expect("to work")
                                .repo_map()
                                .to_owned();

                            human_message_ticker = human_message_ticker + 1;
                            session = session.human_message(
                                human_message_ticker.to_string(),
                                repo_map_str.to_owned(),
                                UserContext::default(),
                                vec![],
                                repo_ref.clone(),
                            );
                            println!("response: {:?}", repo_map_str);
                        }
                    };
                }
                AgentToolUseOutput::Cancelled => {}
                AgentToolUseOutput::Failed(failed_to_parse_output) => {
                    let human_message = format!(
                        r#"Your output was incorrect, please give me the output in the correct format:
{}"#,
                        failed_to_parse_output
                    );
                    human_message_ticker = human_message_ticker + 1;
                    session = session.human_message(
                        human_message_ticker.to_string(),
                        human_message,
                        UserContext::default(),
                        vec![],
                        repo_ref.clone(),
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn code_edit_agentic(
        &self,
        session_id: String,
        storage_path: String,
        scratch_pad_agent: ScratchPadAgent,
        exchange_id: String,
        edit_request: String,
        user_context: UserContext,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        root_directory: String,
        codebase_search: bool,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        println!("session_service::code_edit::agentic::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session(
                session_id.to_owned(),
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                user_context.clone(),
            )
        };

        // add an exchange that we are going to perform anchored edits
        session = session.agentic_edit(exchange_id, edit_request, user_context, codebase_search);

        session = session.accept_open_exchanges_if_any(message_properties.clone());
        let edit_exchange_id = self
            .tool_box
            .create_new_exchange(session_id.to_owned(), message_properties.clone())
            .await?;

        let cancellation_token = tokio_util::sync::CancellationToken::new();
        self.track_exchange(&session_id, &edit_exchange_id, cancellation_token.clone())
            .await;
        message_properties = message_properties
            .set_request_id(edit_exchange_id)
            .set_cancellation_token(cancellation_token);

        session = session
            .perform_agentic_editing(scratch_pad_agent, root_directory, message_properties)
            .await?;

        // save the session to the disk
        self.save_to_storage(&session).await?;
        println!("session_service::code_edit::agentic::stop");
        Ok(())
    }

    /// We are going to try and do code edit since we are donig anchored edit
    pub async fn code_edit_anchored(
        &self,
        session_id: String,
        storage_path: String,
        scratch_pad_agent: ScratchPadAgent,
        exchange_id: String,
        edit_request: String,
        user_context: UserContext,
        project_labels: Vec<String>,
        repo_ref: RepoRef,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        println!("session_service::code_edit::anchored::start");
        let mut session = if let Ok(session) = self.load_from_storage(storage_path.to_owned()).await
        {
            println!(
                "session_service::load_from_storage_ok::session_id({})",
                &session_id
            );
            session
        } else {
            self.create_new_session(
                session_id.to_owned(),
                project_labels.to_vec(),
                repo_ref.clone(),
                storage_path,
                user_context.clone(),
            )
        };

        let selection_variable = user_context.variables.iter().find(|variable| {
            variable.is_selection()
                && !(variable.start_position.line() == 0 && variable.end_position.line() == 0)
        });
        if selection_variable.is_none() {
            return Ok(());
        }
        let selection_variable = selection_variable.expect("is_none to hold above");
        let selection_range = Range::new(
            selection_variable.start_position,
            selection_variable.end_position,
        );
        println!("session_service::selection_range::({:?})", &selection_range);
        let selection_fs_file_path = selection_variable.fs_file_path.to_owned();
        let file_content = self
            .tool_box
            .file_open(
                selection_fs_file_path.to_owned(),
                message_properties.clone(),
            )
            .await?;
        let file_content_in_range = file_content
            .content_in_range(&selection_range)
            .unwrap_or(selection_variable.content.to_owned());

        session = session.accept_open_exchanges_if_any(message_properties.clone());
        let edit_exchange_id = self
            .tool_box
            .create_new_exchange(session_id.to_owned(), message_properties.clone())
            .await?;

        let cancellation_token = tokio_util::sync::CancellationToken::new();
        self.track_exchange(&session_id, &edit_exchange_id, cancellation_token.clone())
            .await;
        message_properties = message_properties
            .set_request_id(edit_exchange_id)
            .set_cancellation_token(cancellation_token);

        // add an exchange that we are going to perform anchored edits
        session = session.anchored_edit(
            exchange_id.to_owned(),
            edit_request,
            user_context,
            selection_range,
            selection_fs_file_path,
            file_content_in_range,
        );

        // Now we can start editing the selection over here
        session = session
            .perform_anchored_edit(
                exchange_id,
                scratch_pad_agent,
                self.tool_box.clone(),
                message_properties,
            )
            .await?;

        // save the session to the disk
        self.save_to_storage(&session).await?;
        println!("session_service::code_edit::anchored_edit::finished");
        Ok(())
    }

    pub async fn handle_session_undo(
        &self,
        exchange_id: &str,
        storage_path: String,
    ) -> Result<(), SymbolError> {
        let session_maybe = self.load_from_storage(storage_path.to_owned()).await;
        if session_maybe.is_err() {
            return Ok(());
        }
        let mut session = session_maybe.expect("is_err to hold");
        session = session.undo_including_exchange_id(&exchange_id).await?;
        self.save_to_storage(&session).await?;
        Ok(())
    }

    /// Provied feedback to the exchange
    ///
    /// We can react to this later on and send out either another exchange or something else
    /// but for now we are just reacting to it on our side so we know
    pub async fn feedback_for_exchange(
        &self,
        exchange_id: &str,
        step_index: Option<usize>,
        accepted: bool,
        storage_path: String,
        tool_box: Arc<ToolBox>,
        mut message_properties: SymbolEventMessageProperties,
    ) -> Result<(), SymbolError> {
        let session_maybe = self.load_from_storage(storage_path.to_owned()).await;
        if session_maybe.is_err() {
            return Ok(());
        }
        let mut session = session_maybe.expect("is_err to hold above");
        session = session
            .react_to_feedback(
                exchange_id,
                step_index,
                accepted,
                message_properties.clone(),
            )
            .await?;
        self.save_to_storage(&session).await?;
        let session_id = session.session_id().to_owned();
        if accepted {
            println!(
                "session_service::feedback_for_exchange::exchange_id({})::accepted::({})",
                &exchange_id, accepted,
            );
            // if we have accepted it, then we can help the user move forward
            // there are many conditions we can handle over here
            let is_hot_streak_worthy_message = session
                .get_exchange_by_id(&exchange_id)
                .map(|exchange| exchange.is_hot_streak_worthy_message())
                .unwrap_or_default();
            // if we can't reply to the message return quickly over here
            if !is_hot_streak_worthy_message {
                return Ok(());
            }
            let hot_streak_exchange_id = self
                .tool_box
                .create_new_exchange(session_id.to_owned(), message_properties.clone())
                .await?;

            let cancellation_token = tokio_util::sync::CancellationToken::new();
            self.track_exchange(
                &session_id,
                &hot_streak_exchange_id,
                cancellation_token.clone(),
            )
            .await;
            message_properties = message_properties
                .set_request_id(hot_streak_exchange_id)
                .set_cancellation_token(cancellation_token);

            // now ask the session_service to generate the next most important step
            // which the agent should take over here
            session
                .hot_streak_message(exchange_id, tool_box, message_properties)
                .await?;
        } else {
            // if we rejected the agent message, then we can ask for feedback so we can
            // work on it
        }
        Ok(())
    }

    /// Returns if the exchange was really cancelled
    pub async fn set_exchange_as_cancelled(
        &self,
        storage_path: String,
        exchange_id: String,
        message_properties: SymbolEventMessageProperties,
    ) -> Result<bool, SymbolError> {
        let mut session = self.load_from_storage(storage_path).await.map_err(|e| {
            println!(
                "session_service::set_exchange_as_cancelled::exchange_id({})::error({:?})",
                &exchange_id, e
            );
            e
        })?;

        let send_cancellation_signal = session.has_running_code_edits(&exchange_id);
        println!(
            "session_service::exchange_id({})::should_cancel::({})",
            &exchange_id, send_cancellation_signal
        );

        session = session.set_exchange_as_cancelled(&exchange_id, message_properties);
        self.save_to_storage(&session).await?;
        Ok(send_cancellation_signal)
    }

    async fn load_from_storage(&self, storage_path: String) -> Result<Session, SymbolError> {
        let content = tokio::fs::read_to_string(storage_path.to_owned())
            .await
            .map_err(|e| SymbolError::IOError(e))?;

        let session: Session = serde_json::from_str(&content).expect(&format!(
            "converting to session from json is okay: {storage_path}"
        ));
        Ok(session)
    }

    async fn save_to_storage(&self, session: &Session) -> Result<(), SymbolError> {
        let serialized = serde_json::to_string(session).unwrap();
        let mut file = tokio::fs::File::create(session.storage_path())
            .await
            .map_err(|e| SymbolError::IOError(e))?;
        file.write_all(serialized.as_bytes())
            .await
            .map_err(|e| SymbolError::IOError(e))?;
        Ok(())
    }
}
