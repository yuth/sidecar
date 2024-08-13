//! Contains the struct for search and replace style editing

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use llm_client::{
    broker::LLMBroker,
    clients::types::{LLMClientCompletionRequest, LLMClientMessage},
};

use crate::{
    agentic::{
        symbol::{
            identifier::{LLMProperties, SymbolIdentifier},
            ui_event::UIEventWithID,
        },
        tool::{errors::ToolError, input::ToolInput, output::ToolOutput, r#type::Tool},
    },
    chunking::text_document::{Position, Range},
};

const SURROUNDING_CONTEXT_LIMIT: usize = 200;

#[derive(Debug)]
pub struct SearchAndReplaceEditingResponse {
    response: String,
}

impl SearchAndReplaceEditingResponse {
    pub fn new(response: String) -> Self {
        Self { response }
    }

    pub fn response(&self) -> &str {
        &self.response
    }
}

#[derive(Debug, Clone)]
pub struct SearchAndReplaceEditingRequest {
    fs_file_path: String,
    // TODO(skcd): we use this to detect the range where we want to perform the edits
    edit_range: Range,
    context_in_edit_selection: String,
    code_above: Option<String>,
    code_below: Option<String>,
    extra_data: String,
    llm_properties: LLMProperties,
    language: String,
    new_symbols: Option<String>,
    instructions: String,
    root_request_id: String,
    symbol_identifier: SymbolIdentifier,
    edit_request_id: String,
    ui_sender: UnboundedSender<UIEventWithID>,
}

impl SearchAndReplaceEditingRequest {
    pub fn new(
        fs_file_path: String,
        edit_range: Range,
        context_in_edit_selection: String,
        code_above: Option<String>,
        code_below: Option<String>,
        extra_data: String,
        llm_properties: LLMProperties,
        language: String,
        new_symbols: Option<String>,
        instructions: String,
        root_request_id: String,
        symbol_identifier: SymbolIdentifier,
        edit_request_id: String,
        ui_sender: UnboundedSender<UIEventWithID>,
    ) -> Self {
        Self {
            fs_file_path,
            edit_range,
            context_in_edit_selection,
            code_above,
            code_below,
            extra_data,
            llm_properties,
            language,
            new_symbols,
            instructions,
            root_request_id,
            symbol_identifier,
            edit_request_id,
            ui_sender,
        }
    }
}

pub struct SearchAndReplaceEditing {
    llm_client: Arc<LLMBroker>,
    _fail_over_llm: LLMProperties,
}

impl SearchAndReplaceEditing {
    pub fn new(llm_client: Arc<LLMBroker>, fail_over_llm: LLMProperties) -> Self {
        Self {
            llm_client,
            _fail_over_llm: fail_over_llm,
        }
    }

    fn system_message(&self, language: &str) -> String {
        format!(r#"Act as an expert software developer.
Always use best practices when coding.
Respect and use existing conventions, libraries, etc that are already present in the code base.
You are diligent and tireless!
You NEVER leave comments describing code without implementing it!
You always COMPLETELY IMPLEMENT the needed code!
You will be presented with a single file and the code which you can EDIT will be given in a <code_to_edit_section>.
You will be also provided with some extra data, which contains various definitions of symbols which you can use to use the call the correct functions and re-use existing functionality in the code.
Take requests for changes to the supplied code.
If the request is ambiguous, ask questions.

Always reply to the user in the same language they are using.

Once you understand the request you MUST:
1. Decide if you need to propose *SEARCH/REPLACE* edits to any files that haven't been added to the chat. You can create new files without asking. But if you need to propose edits to existing files not already added to the chat, you *MUST* tell the user their full path names and ask them to *add the files to the chat*. End your reply and wait for their approval. You can keep asking if you then decide you need to edit more files.
2. Think step-by-step and explain the needed changes with a numbered list of short sentences put this in a xml tag called <thinking> at the very start of your answer.
3. Describe each change with a *SEARCH/REPLACE block* per the examples below. All changes to files must use this *SEARCH/REPLACE block* format. ONLY EVER RETURN CODE IN A *SEARCH/REPLACE BLOCK*!

All changes to files must use the *SEARCH/REPLACE block* format.

# *SEARCH/REPLACE block* Rules:

Every *SEARCH/REPLACE block* must use this format:
1. The file path alone on a line, verbatim. No bold asterisks, no quotes around it, no escaping of characters, etc.
2. The opening fence and code language, eg: ```{language}
3. The start of search block: <<<<<<< SEARCH
4. A contiguous chunk of lines to search for in the existing source code
5. The dividing line: =======
6. The lines to replace into the source code
7. The end of the replace block: >>>>>>> REPLACE
8. The closing fence: ```

Every *SEARCH* section must *EXACTLY MATCH* the existing source code, character for character, including all comments, docstrings, etc.


*SEARCH/REPLACE* blocks will replace *all* matching occurrences.
Include enough lines to make the SEARCH blocks uniquely match the lines to change.

Keep *SEARCH/REPLACE* blocks concise.
Break large *SEARCH/REPLACE* blocks into a series of smaller blocks that each change a small portion of the file.
Include just the changing lines, and a few surrounding lines if needed for uniqueness.
Do not include long runs of unchanging lines in *SEARCH/REPLACE* blocks.

Only create *SEARCH/REPLACE* blocks for files that the user has added to the chat!

To move code within a file, use 2 *SEARCH/REPLACE* blocks: 1 to delete it from its current location, 1 to insert it in the new location.

If you want to put code in a new file, use a *SEARCH/REPLACE block* with:
- A new file path, including dir name if needed
- An empty `SEARCH` section
- The new file's contents in the `REPLACE` section

You are diligent and tireless!
You NEVER leave comments describing code without implementing it!
You always COMPLETELY IMPLEMENT the needed code!
ONLY EVER RETURN CODE IN A *SEARCH/REPLACE BLOCK*!
You always put your thinking in <thinking> section before you suggest *SEARCH/REPLACE* blocks"#).to_owned()
    }

    fn extra_data(&self, extra_data: &str) -> String {
        format!(
            r#"This is the extra data which you can use:
<extra_data>
{extra_data}
</extra_data>"#
        )
    }

    fn above_selection(&self, above_selection: Option<&str>) -> Option<String> {
        if let Some(above_selection) = above_selection {
            Some(format!(
                r#"<code_above>
{above_selection}
</code_above>"#
            ))
        } else {
            None
        }
    }

    fn below_selection(&self, below_selection: Option<&str>) -> Option<String> {
        if let Some(below_selection) = below_selection {
            Some(format!(
                r#"<code_below>
{below_selection}
</code_below>"#
            ))
        } else {
            None
        }
    }

    fn selection_to_edit(&self, selection_to_edit: &str) -> String {
        format!(
            r#"<code_to_edit_selection>
{selection_to_edit}
</code_to_edit_selection>"#
        )
    }

    fn user_message(&self, context: SearchAndReplaceEditingRequest) -> String {
        let extra_data = self.extra_data(&context.extra_data);
        let above = self.above_selection(
            context
                .code_above
                .map(|code_above| {
                    // limit it to 100 lines from the start
                    let mut lines = code_above.lines().collect::<Vec<_>>();
                    lines.reverse();
                    lines.truncate(SURROUNDING_CONTEXT_LIMIT);
                    lines.reverse();
                    lines.join("\n")
                })
                .as_deref(),
        );
        let below = self.below_selection(
            context
                .code_below
                .map(|code_below| {
                    let mut lines = code_below.lines().collect::<Vec<_>>();
                    lines.truncate(SURROUNDING_CONTEXT_LIMIT / 3);
                    lines.join("\n")
                })
                .as_deref(),
        );
        let in_range = self.selection_to_edit(&context.context_in_edit_selection);
        let mut user_message = "".to_owned();
        if let Some(extra_symbols) = context.new_symbols.clone() {
            user_message = user_message
                + &format!(
                    r#"<extra_symbols_will_be_created>
{extra_symbols}
</extra_symbols_will_be_created>"#
                );
        }
        user_message = user_message + &extra_data + "\n";
        if let Some(above) = above {
            user_message = user_message + &above + "\n";
        }
        if let Some(below) = below {
            user_message = user_message + &below + "\n";
        }
        user_message = user_message + &in_range + "\n";
        let instructions = context.instructions;
        let fs_file_path = context.fs_file_path;
        user_message = user_message
            + &format!(
                r#"Only edit the code in <code_to_edit_selection> my instructions are:
<user_instruction>
{instructions}
</user_insturction>

<fs_file_path>
{fs_file_path}
</fs_file_path>"#
            );
        user_message
    }

    fn example_messages(&self) -> Vec<LLMClientMessage> {
        vec![
            LLMClientMessage::user(r#"Change get_factorial() to use math.factorial"#.to_owned()),
            LLMClientMessage::assistant(
                r#"<thinking>
To make this change we need to modify `mathweb/flask/app.py` to:

1. Import the math package.
2. Remove the existing factorial() function.
3. Update get_factorial() to call math.factorial instead.
</thinking>

Here are the *SEARCH/REPLACE* blocks:

mathweb/flask/app.py
```python
<<<<<<< SEARCH
from flask import Flask
=======
import math
from flask import Flask
>>>>>>> REPLACE
```

mathweb/flask/app.py
```python
<<<<<<< SEARCH
def factorial(n):
    "compute factorial"

    if n == 0:
        return 1
    else:
        return n * factorial(n-1)

=======
>>>>>>> REPLACE
```

mathweb/flask/app.py
```python
<<<<<<< SEARCH
    return str(factorial(n))
=======
    return str(math.factorial(n))
>>>>>>> REPLACE
```"#
                    .to_owned(),
            ),
        ]
    }
}

#[async_trait]
impl Tool for SearchAndReplaceEditing {
    async fn invoke(&self, input: ToolInput) -> Result<ToolOutput, ToolError> {
        let context = input.should_search_and_replace_editing()?;
        let code_to_edit = context.context_in_edit_selection.to_owned();
        let code_to_edit_range = context.edit_range.clone();
        let symbol_identifier = context.symbol_identifier.clone();
        let ui_sender = context.ui_sender.clone();
        let fs_file_path = context.fs_file_path.to_owned();
        let edit_request_id = context.edit_request_id.to_owned();
        let llm_properties = context.llm_properties.clone();
        let root_request_id = context.root_request_id.to_owned();
        let system_message = LLMClientMessage::system(self.system_message(&context.language));
        let user_message = LLMClientMessage::user(self.user_message(context));
        let example_messages = self.example_messages();
        let request = LLMClientCompletionRequest::new(
            llm_properties.llm().clone(),
            vec![system_message]
                .into_iter()
                .chain(example_messages)
                .chain(vec![user_message])
                .collect(),
            0.2,
            None,
        );
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut llm_response = Box::pin(
            self.llm_client.stream_completion(
                llm_properties.api_key().clone(),
                request,
                llm_properties.provider().clone(),
                vec![
                    (
                        "event_type".to_owned(),
                        "search_and_replace_editing".to_owned(),
                    ),
                    ("root_id".to_owned(), root_request_id.to_owned()),
                ]
                .into_iter()
                .collect(),
                sender,
            ),
        );
        let stream_result;

        let (edits_sender, mut edits_receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut search_and_replace_accumulator = SearchAndReplaceAccumulator::new(
            code_to_edit,
            code_to_edit_range.start_line(),
            edits_sender,
        );

        // now we can bring it all together and use the answer accumulator over here
        // to start the processing completely

        loop {
            tokio::select! {
                stream_msg = receiver.recv() => {
                    match stream_msg {
                        Some(msg) => {
                            let delta = msg.delta();
                            if let Some(delta) = delta {
                                // we have some delta over here which we can process
                                search_and_replace_accumulator.add_delta(delta.to_owned());
                            }
                        }
                        None => {
                            // we should flush the accumualtor over here
                            // channel is probably closed over here?
                        },
                    }
                }
                edits_response = edits_receiver.recv() => {
                    match edits_response {
                        Some(EditDelta::EditStarted(range)) => {
                            let _ = ui_sender.send(UIEventWithID::start_edit_streaming(
                                root_request_id.to_owned(),
                                symbol_identifier.clone(),
                                edit_request_id.to_owned(),
                                range,
                                fs_file_path.to_owned(),
                            ));
                            // we need to send this ``` since thats the detection string
                            // we use for making sure that we are inside a code-block on the
                            // editor
                            let _ = ui_sender.send(UIEventWithID::delta_edit_streaming(
                                root_request_id.to_owned(),
                                symbol_identifier.clone(),
                                "```\n".to_owned(),
                                edit_request_id.to_owned(),
                                range,
                                fs_file_path.to_owned(),
                            ));
                        }
                        Some(EditDelta::EditDelta((range, delta))) => {
                            let _ = ui_sender.send(UIEventWithID::delta_edit_streaming(
                                root_request_id.to_owned(),
                                symbol_identifier.clone(),
                                delta,
                                edit_request_id.to_owned(),
                                range,
                                fs_file_path.to_owned(),
                            ));
                        }
                        Some(EditDelta::EditEnd(range)) => {
                            let _ = ui_sender.send(UIEventWithID::delta_edit_streaming(
                                root_request_id.to_owned(),
                                symbol_identifier.clone(),
                                "\n```".to_owned(),
                                edit_request_id.to_owned(),
                                range,
                                fs_file_path.to_owned(),
                            ));
                            let _ = ui_sender.send(UIEventWithID::end_edit_streaming(
                                root_request_id.to_owned(),
                                symbol_identifier.clone(),
                                edit_request_id.to_owned(),
                                range,
                                fs_file_path.to_owned(),
                            ));
                        }
                        None => {

                        }
                    }
                }
                result = &mut llm_response => {
                    stream_result = Some(result);
                    break;
                }
            }
        }
        match stream_result {
            Some(Ok(response)) => Ok(ToolOutput::search_and_replace_editing(
                SearchAndReplaceEditingResponse::new(response),
            )),
            // wrong error over here but its fine for now
            _ => Err(ToolError::RetriesExhausted),
        }
    }
}

enum EditDelta {
    EditStarted(Range),
    EditDelta((Range, String)),
    EditEnd(Range),
}

#[derive(Debug, Clone)]
enum SearchBlockStatus {
    NoBlock,
    BlockStart,
    BlockAccumulate(String),
    BlockFound((String, Range)),
}

struct SearchAndReplaceAccumulator {
    code_lines: Vec<String>,
    start_line: usize,
    answer_up_until_now: String,
    previous_answer_line_number: Option<usize>,
    search_block_status: SearchBlockStatus,
    updated_block: Option<String>,
    sender: UnboundedSender<EditDelta>,
}

impl SearchAndReplaceAccumulator {
    pub fn new(
        code_to_edit: String,
        start_line: usize,
        sender: UnboundedSender<EditDelta>,
    ) -> Self {
        Self {
            code_lines: code_to_edit
                .lines()
                .into_iter()
                .map(|line| line.to_owned())
                .collect::<Vec<_>>(),
            start_line,
            answer_up_until_now: "".to_owned(),
            previous_answer_line_number: None,
            search_block_status: SearchBlockStatus::NoBlock,
            updated_block: None,
            sender,
        }
    }

    fn add_delta(&mut self, delta: String) {
        self.answer_up_until_now.push_str(&delta);
        self.process_answer();
        // check if we have a new search block starting here
    }

    fn process_answer(&mut self) {
        // so there are 2 cases over here which we want to handle
        // - we haven't even started processing the lines yet which sucks kinda
        // - we have started processing the lines but we do not have any lines with us
        // right now
        let head = "<<<<<<< SEARCH";
        let divider = "=======";
        let updated = ">>>>>>> REPLACE";

        // no complete line right now, keep going
        let line_number_to_process = get_last_newline_line_number(&self.answer_up_until_now);
        if line_number_to_process.is_none() {
            return;
        }
        // we get this value as the last line number always, so better to subtract here and if its none we are returning early
        let line_number_to_process_until = line_number_to_process.expect("to work") - 1;
        let answer_lines = self
            .answer_up_until_now
            .lines()
            .into_iter()
            .collect::<Vec<_>>();

        // line number we can proceed until:
        let start_index = if self.previous_answer_line_number.is_none() {
            0
        } else {
            self.previous_answer_line_number
                .expect("if_none above to work")
                + 1
        };

        // println!("process_answer::start_index({})", start_index);

        for line_number in start_index..=line_number_to_process_until {
            // update our answer lines right now
            self.previous_answer_line_number = Some(line_number);
            let answer_line_at_index = answer_lines[line_number];
            // println!(
            //     "process_answer::line_number({})::block_status({:?})::line_content({})",
            //     line_number, &self.search_block_status, &answer_line_at_index
            // );
            // clone here is bad, we should try and get rid of it
            match self.search_block_status.clone() {
                SearchBlockStatus::NoBlock => {
                    if answer_line_at_index == head {
                        self.search_block_status = SearchBlockStatus::BlockStart;
                    }
                    continue;
                }
                SearchBlockStatus::BlockStart => {
                    self.search_block_status =
                        SearchBlockStatus::BlockAccumulate(answer_line_at_index.to_owned());
                }
                SearchBlockStatus::BlockAccumulate(accumulated) => {
                    if answer_line_at_index == divider {
                        // we also have to find the range in the code where this block is present
                        // since that will be our edit range
                        let range = get_range_for_search_block(
                            &self.code_lines.join("\n"),
                            self.start_line,
                            &accumulated,
                        );
                        match range {
                            Some(range) => {
                                self.search_block_status = SearchBlockStatus::BlockFound((
                                    accumulated.to_owned(),
                                    range.clone(),
                                ));
                                let _ = self.sender.send(EditDelta::EditStarted(range));
                            }
                            None => {
                                // if we do not find any replacement block, then we give up
                                // and keep going forward for now
                                self.search_block_status = SearchBlockStatus::NoBlock;
                            }
                        };
                    } else {
                        self.search_block_status = SearchBlockStatus::BlockAccumulate(format!(
                            "{}\n{}",
                            accumulated, answer_line_at_index
                        ));
                    }
                }
                SearchBlockStatus::BlockFound((_, block_range)) => {
                    if answer_line_at_index == updated {
                        // neat we found when to close, so we can do that now
                        // return an event which stops the edit stream
                        self.search_block_status = SearchBlockStatus::NoBlock;
                        // we need to update the answer lines with the new replace block
                        if let Some(updated_answer) = self.updated_block.clone() {
                            let updated_range_start_line =
                                block_range.start_line() - self.start_line;
                            let updated_range_end_line = block_range.end_line() - self.start_line;
                            let mut updated_code_lines =
                                self.code_lines[..updated_range_start_line].join("\n");
                            updated_code_lines.push('\n');
                            updated_code_lines.push_str(&updated_answer);
                            updated_code_lines.push('\n');
                            updated_code_lines
                                .push_str(&self.code_lines[updated_range_end_line..].join("\n"));
                            self.code_lines = updated_code_lines
                                .lines()
                                .into_iter()
                                .map(|line| line.to_owned())
                                .collect::<Vec<_>>();
                        }
                        self.updated_block = None;
                        let _ = self.sender.send(EditDelta::EditEnd(block_range.clone()));
                    } else {
                        if self.updated_block.is_none() {
                            self.updated_block = Some(answer_line_at_index.to_owned());
                            let _ = self.sender.send(EditDelta::EditDelta((
                                block_range.clone(),
                                answer_line_at_index.to_owned(),
                            )));
                        } else {
                            self.updated_block = Some(
                                self.updated_block.clone().expect("is_none to hold")
                                    + "\n"
                                    + answer_line_at_index,
                            );
                            let _ = self.sender.send(EditDelta::EditDelta((
                                block_range.clone(),
                                ("\n".to_owned() + answer_line_at_index).to_owned(),
                            )));
                        }
                    }
                }
            }
        }
    }
}

/// Hels to get the last line number which has a \n
fn get_last_newline_line_number(s: &str) -> Option<usize> {
    s.rfind('\n')
        .map(|last_index| s[..=last_index].chars().filter(|&c| c == '\n').count())
}

fn get_range_for_search_block(
    code_to_look_at: &str,
    start_line: usize,
    search_block: &str,
) -> Option<Range> {
    let code_to_look_at_lines = code_to_look_at
        .lines()
        .into_iter()
        .enumerate()
        .map(|(idx, line)| (idx + start_line, line.to_owned()))
        .collect::<Vec<_>>();

    let search_block_lines = search_block.lines().into_iter().collect::<Vec<_>>();
    let search_block_len = search_block_lines.len();
    for i in 0..=code_to_look_at_lines.len() - search_block_len {
        if code_to_look_at_lines[i..i + search_block_len]
            .iter()
            .map(|(_, content)| content)
            .collect::<Vec<_>>()
            == search_block_lines
        {
            // we have our answer over here, now return the range
            return Some(Range::new(
                Position::new(code_to_look_at_lines[i].0, 0, 0),
                Position::new(code_to_look_at_lines[i + search_block_len - 1].0, 0, 0),
            ));
        }
    }
    None
}
