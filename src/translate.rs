use serde_json::{Map, Value, json};

#[derive(Clone, Copy)]
pub enum Dir {
    MessagesToChat,
    ChatToMessages,
}

impl Dir {
    pub fn target_endpoint(self) -> &'static str {
        match self {
            Dir::MessagesToChat => "v1/chat/completions",
            Dir::ChatToMessages => "v1/messages",
        }
    }

    pub fn target_is_anthropic(self) -> bool {
        matches!(self, Dir::ChatToMessages)
    }
}

pub fn map_request(dir: Dir, body: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let src: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let stream = src.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let out = match dir {
        Dir::MessagesToChat => messages_to_chat_req(&src),
        Dir::ChatToMessages => chat_to_messages_req(&src),
    };
    serde_json::to_vec(&out)
        .map(|b| (b, stream))
        .map_err(|e| e.to_string())
}

pub fn map_response(dir: Dir, body: &[u8]) -> Result<Vec<u8>, String> {
    let src: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let out = match dir {
        Dir::MessagesToChat => chat_to_messages_resp(&src),
        Dir::ChatToMessages => messages_to_chat_resp(&src),
    };
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

fn messages_to_chat_req(src: &Value) -> Value {
    let mut messages = Vec::new();
    match src.get("system") {
        Some(Value::String(s)) => messages.push(json!({"role": "system", "content": s})),
        Some(Value::Array(blocks)) => {
            let text = join_text(blocks);
            if !text.is_empty() {
                messages.push(json!({"role": "system", "content": text}));
            }
        }
        _ => {}
    }
    for m in array(src.get("messages")) {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        push_anth_msg(role, m.get("content"), &mut messages);
    }

    let mut out = Map::new();
    out.insert(
        "model".into(),
        src.get("model").cloned().unwrap_or(Value::Null),
    );
    out.insert("messages".into(), Value::Array(messages));
    if let Some(mt) = src.get("max_tokens") {
        out.insert("max_tokens".into(), mt.clone());
    }
    let stream = src.get("stream").and_then(Value::as_bool).unwrap_or(false);
    copy(&mut out, src, "temperature");
    copy(&mut out, src, "top_p");
    copy(&mut out, src, "stream");
    if let Some(stops) = src.get("stop_sequences") {
        out.insert("stop".into(), stops.clone());
    }
    if let Some(tools) = src.get("tools").and_then(Value::as_array) {
        out.insert(
            "tools".into(),
            tools.iter().map(anth_tool_to_openai).collect(),
        );
    }
    if let Some(tc) = src.get("tool_choice") {
        out.insert("tool_choice".into(), anth_tool_choice(tc));
    }
    if stream {
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Value::Object(out)
}

fn push_anth_msg(role: &str, content: Option<&Value>, out: &mut Vec<Value>) {
    let blocks = match content {
        Some(Value::String(s)) => {
            out.push(json!({"role": role, "content": s}));
            return;
        }
        Some(Value::Array(blocks)) => blocks,
        _ => return,
    };
    if role == "assistant" {
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => tool_calls.push(json!({
                    "id": b.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": b.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": b.get("input").map(to_json_string).unwrap_or_default(),
                    }
                })),
                _ => {}
            }
        }
        let mut msg = Map::new();
        msg.insert("role".into(), "assistant".into());
        msg.insert(
            "content".into(),
            if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            },
        );
        if !tool_calls.is_empty() {
            msg.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        out.push(Value::Object(msg));
    } else {
        let mut parts = Vec::new();
        let mut tool_msgs = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    json!({"type": "text", "text": b.get("text").cloned().unwrap_or(Value::Null)}),
                ),
                Some("image") => {
                    if let Some(url) = image_data_url(b) {
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                }
                Some("tool_result") => tool_msgs.push(json!({
                    "role": "tool",
                    "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(Value::Null),
                    "content": tool_result_content(b.get("content")),
                })),
                _ => {}
            }
        }
        out.extend(tool_msgs);
        if !parts.is_empty() {
            let content = if parts.len() == 1 && parts[0]["type"] == "text" {
                parts[0]["text"].clone()
            } else {
                Value::Array(parts)
            };
            out.push(json!({"role": "user", "content": content}));
        }
    }
}

fn chat_to_messages_req(src: &Value) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in array(src.get("messages")) {
        match m.get("role").and_then(Value::as_str).unwrap_or("user") {
            "system" => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&openai_text(m.get("content")));
            }
            "assistant" => {
                let mut blocks = Vec::new();
                let text = openai_text(m.get("content"));
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for tc in array(m.get("tool_calls")) {
                    let f = tc.get("function");
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "name": f.and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null),
                        "input": parse_args(f.and_then(|f| f.get("arguments"))),
                    }));
                }
                push_turn(&mut messages, "assistant", blocks);
            }
            "tool" => push_turn(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "content": openai_text(m.get("content")),
                })],
            ),
            _ => push_turn(&mut messages, "user", openai_user_blocks(m.get("content"))),
        }
    }

    let mut out = Map::new();
    out.insert(
        "model".into(),
        src.get("model").cloned().unwrap_or(Value::Null),
    );
    let max = src
        .get("max_completion_tokens")
        .or_else(|| src.get("max_tokens"))
        .cloned()
        .unwrap_or(json!(4096));
    out.insert("max_tokens".into(), max);
    if !system.is_empty() {
        out.insert("system".into(), Value::String(system));
    }
    out.insert("messages".into(), Value::Array(messages));
    copy(&mut out, src, "temperature");
    copy(&mut out, src, "top_p");
    copy(&mut out, src, "stream");
    if let Some(stop) = src.get("stop") {
        let seqs = match stop {
            Value::String(s) => json!([s]),
            other => other.clone(),
        };
        out.insert("stop_sequences".into(), seqs);
    }
    if let Some(tools) = src.get("tools").and_then(Value::as_array) {
        let t: Vec<Value> = tools.iter().filter_map(openai_tool_to_anth).collect();
        if !t.is_empty() {
            out.insert("tools".into(), Value::Array(t));
        }
    }
    if let Some(tc) = src.get("tool_choice") {
        out.insert("tool_choice".into(), openai_tool_choice(tc));
    }
    Value::Object(out)
}

fn push_turn(msgs: &mut Vec<Value>, role: &str, mut blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(Value::Array(arr)) = last.get_mut("content")
    {
        arr.append(&mut blocks);
        return;
    }
    msgs.push(json!({"role": role, "content": blocks}));
}

fn chat_to_messages_resp(src: &Value) -> Value {
    let choice = src
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let msg = choice.and_then(|c| c.get("message"));
    let mut content = Vec::new();
    if let Some(text) = msg.and_then(|m| m.get("content")).and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    for tc in array(msg.and_then(|m| m.get("tool_calls"))) {
        let f = tc.get("function");
        content.push(json!({
            "type": "tool_use",
            "id": tc.get("id").cloned().unwrap_or(Value::Null),
            "name": f.and_then(|f| f.get("name")).cloned().unwrap_or(Value::Null),
            "input": parse_args(f.and_then(|f| f.get("arguments"))),
        }));
    }
    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);
    let usage = src.get("usage");
    json!({
        "id": src.get("id").cloned().unwrap_or_else(|| json!("msg_0")),
        "type": "message",
        "role": "assistant",
        "model": src.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": openai_finish_to_anth(finish),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("prompt_tokens")).cloned().unwrap_or(json!(0)),
            "output_tokens": usage.and_then(|u| u.get("completion_tokens")).cloned().unwrap_or(json!(0)),
        }
    })
}

fn messages_to_chat_resp(src: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for b in array(src.get("content")) {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": b.get("id").cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": b.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": b.get("input").map(to_json_string).unwrap_or_default(),
                },
            })),
            _ => {}
        }
    }
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    let usage = src.get("usage");
    let pt = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let ct = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json!({
        "id": src.get("id").cloned().unwrap_or_else(|| json!("chatcmpl_0")),
        "object": "chat.completion",
        "created": 0,
        "model": src.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": anth_stop_to_openai(src.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": {"prompt_tokens": pt, "completion_tokens": ct, "total_tokens": pt + ct},
    })
}

pub struct SseXlate {
    buf: Vec<u8>,
    done: bool,
    state: State,
}

enum State {
    ToMessages(ToMsg),
    ToChat(ToChat),
}

impl SseXlate {
    pub fn new(dir: Dir) -> Self {
        let state = match dir {
            Dir::MessagesToChat => State::ToMessages(ToMsg::default()),
            Dir::ChatToMessages => State::ToChat(ToChat::default()),
        };
        Self {
            buf: Vec::new(),
            done: false,
            state,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = raw[..raw.len() - 1]
                .strip_suffix(b"\r")
                .unwrap_or(&raw[..raw.len() - 1]);
            self.handle_line(line, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let raw = std::mem::take(&mut self.buf);
            let line = raw.strip_suffix(b"\r").unwrap_or(&raw);
            self.handle_line(line, &mut out);
        }
        self.finalize(&mut out);
        out
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii_start();
        if data == b"[DONE]" {
            self.finalize(out);
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        match &mut self.state {
            State::ToMessages(s) => s.event(&v, out),
            State::ToChat(s) => s.event(&v, out),
        }
    }

    fn finalize(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        self.done = true;
        match &mut self.state {
            State::ToMessages(s) => s.finish(out),
            State::ToChat(s) => s.finish(out),
        }
    }
}

#[derive(Default)]
struct ToMsg {
    started: bool,
    open: Option<Block>,
    next_index: usize,
    stop: Option<String>,
    output_tokens: Option<i64>,
    id: Option<String>,
    model: Option<String>,
}

struct Block {
    index: usize,
    tool: Option<i64>,
}

impl ToMsg {
    fn event(&mut self, v: &Value, out: &mut Vec<u8>) {
        if !self.started {
            self.id = v.get("id").and_then(Value::as_str).map(String::from);
            self.model = v.get("model").and_then(Value::as_str).map(String::from);
            self.emit_start(out);
        }
        if let Some(ct) = v
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_i64)
        {
            self.output_tokens = Some(ct);
        }
        let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(fr.to_string());
        }
        let delta = choice.get("delta");
        if let Some(text) = delta.and_then(|d| d.get("content")).and_then(Value::as_str)
            && !text.is_empty()
        {
            self.ensure_text(out);
            let index = self.open.as_ref().unwrap().index;
            anth_event(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "text_delta", "text": text},
                }),
            );
        }
        for tc in array(delta.and_then(|d| d.get("tool_calls"))) {
            let oai = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
            if self.open.as_ref().is_none_or(|o| o.tool != Some(oai)) {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                let name = tc
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.open_tool(oai, id, name, out);
            }
            if let Some(args) = tc.pointer("/function/arguments").and_then(Value::as_str)
                && !args.is_empty()
            {
                let index = self.open.as_ref().unwrap().index;
                anth_event(
                    out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": args},
                    }),
                );
            }
        }
    }

    fn emit_start(&mut self, out: &mut Vec<u8>) {
        self.started = true;
        anth_event(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id.clone().unwrap_or_else(|| "msg_stream".into()),
                    "type": "message",
                    "role": "assistant",
                    "model": self.model.clone().unwrap_or_default(),
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        );
    }

    fn close_open(&mut self, out: &mut Vec<u8>) {
        if let Some(o) = self.open.take() {
            anth_event(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": o.index}),
            );
        }
    }

    fn ensure_text(&mut self, out: &mut Vec<u8>) {
        if matches!(&self.open, Some(o) if o.tool.is_none()) {
            return;
        }
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        anth_event(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        );
        self.open = Some(Block { index, tool: None });
    }

    fn open_tool(&mut self, oai: i64, id: &str, name: &str, out: &mut Vec<u8>) {
        self.close_open(out);
        let index = self.next_index;
        self.next_index += 1;
        anth_event(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            }),
        );
        self.open = Some(Block {
            index,
            tool: Some(oai),
        });
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        if !self.started {
            self.emit_start(out);
        }
        self.close_open(out);
        anth_event(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": openai_finish_to_anth(self.stop.as_deref()), "stop_sequence": Value::Null},
                "usage": {"output_tokens": self.output_tokens.unwrap_or(0)},
            }),
        );
        anth_event(out, "message_stop", &json!({"type": "message_stop"}));
    }
}

#[derive(Default)]
struct ToChat {
    started: bool,
    tool_count: i64,
    cur_tool: Option<i64>,
    finish: Option<String>,
    id: Option<String>,
    model: Option<String>,
}

impl ToChat {
    fn event(&mut self, v: &Value, out: &mut Vec<u8>) {
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.id = v
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(String::from);
                self.model = v
                    .pointer("/message/model")
                    .and_then(Value::as_str)
                    .map(String::from);
                self.chunk(out, json!({"role": "assistant"}), Value::Null);
            }
            Some("content_block_start") => {
                let cb = v.get("content_block");
                if cb.and_then(|c| c.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    let oai = self.tool_count;
                    self.tool_count += 1;
                    self.cur_tool = Some(oai);
                    self.chunk(out, json!({"tool_calls": [{
                        "index": oai,
                        "id": cb.and_then(|c| c.get("id")).cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {"name": cb.and_then(|c| c.get("name")).cloned().unwrap_or(Value::Null), "arguments": ""},
                    }]}), Value::Null);
                }
            }
            Some("content_block_delta") => {
                let d = v.get("delta");
                match d.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        let t = d
                            .and_then(|d| d.get("text"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        self.chunk(out, json!({"content": t}), Value::Null);
                    }
                    Some("input_json_delta") => {
                        let pj = d
                            .and_then(|d| d.get("partial_json"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let idx = self.cur_tool.unwrap_or(0);
                        self.chunk(
                            out,
                            json!({"tool_calls": [{"index": idx, "function": {"arguments": pj}}]}),
                            Value::Null,
                        );
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish = Some(sr.to_string());
                }
            }
            _ => {}
        }
    }

    fn chunk(&mut self, out: &mut Vec<u8>, delta: Value, finish: Value) {
        self.started = true;
        openai_event(
            out,
            &json!({
                "id": self.id.clone().unwrap_or_else(|| "chatcmpl_stream".into()),
                "object": "chat.completion.chunk",
                "created": 0,
                "model": self.model.clone().unwrap_or_default(),
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }),
        );
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        let fr = anth_stop_to_openai(self.finish.as_deref());
        self.chunk(out, json!({}), fr);
        out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

fn array(v: Option<&Value>) -> impl Iterator<Item = &Value> {
    v.and_then(Value::as_array).into_iter().flatten()
}

fn copy(dst: &mut Map<String, Value>, src: &Value, key: &str) {
    if let Some(v) = src.get(key)
        && !v.is_null()
    {
        dst.insert(key.into(), v.clone());
    }
}

fn join_text(blocks: &[Value]) -> String {
    let mut s = String::new();
    for b in blocks {
        if b.get("type").and_then(Value::as_str) == Some("text") {
            s.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""));
        }
    }
    s
}

fn to_json_string(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn parse_args(a: Option<&Value>) -> Value {
    match a {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        Some(v) => v.clone(),
        None => json!({}),
    }
}

fn image_data_url(b: &Value) -> Option<String> {
    let src = b.get("source")?;
    match src.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let mt = src.get("media_type").and_then(Value::as_str)?;
            let data = src.get("data").and_then(Value::as_str)?;
            Some(format!("data:{mt};base64,{data}"))
        }
        Some("url") => src.get("url").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

fn tool_result_content(c: Option<&Value>) -> Value {
    match c {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut parts = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some("image") => {
                        if let Some(url) = image_data_url(b) {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                Value::String(text)
            } else {
                if !text.is_empty() {
                    parts.insert(0, json!({"type": "text", "text": text}));
                }
                Value::Array(parts)
            }
        }
        Some(other) => Value::String(to_json_string(other)),
        None => Value::String(String::new()),
    }
}

fn openai_text(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => join_text(parts),
        _ => String::new(),
    }
}

fn openai_user_blocks(c: Option<&Value>) -> Vec<Value> {
    match c {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => Some(
                    json!({"type": "text", "text": p.get("text").cloned().unwrap_or(Value::Null)}),
                ),
                Some("image_url") => {
                    let url = p.pointer("/image_url/url").and_then(Value::as_str)?;
                    Some(anth_image_block(url))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn anth_image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((meta, data)) = rest.split_once(',')
    {
        let media_type = meta.strip_suffix(";base64").unwrap_or(meta);
        return json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}});
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

fn anth_tool_to_openai(t: &Value) -> Value {
    let mut f = Map::new();
    f.insert("name".into(), t.get("name").cloned().unwrap_or(Value::Null));
    if let Some(d) = t.get("description") {
        f.insert("description".into(), d.clone());
    }
    f.insert(
        "parameters".into(),
        t.get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    json!({"type": "function", "function": Value::Object(f)})
}

fn openai_tool_to_anth(t: &Value) -> Option<Value> {
    let f = t.get("function")?;
    let mut m = Map::new();
    m.insert("name".into(), f.get("name").cloned().unwrap_or(Value::Null));
    if let Some(d) = f.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert(
        "input_schema".into(),
        f.get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    );
    Some(Value::Object(m))
}

fn anth_tool_choice(tc: &Value) -> Value {
    match tc.get("type").and_then(Value::as_str) {
        Some("any") => "required".into(),
        Some("none") => "none".into(),
        Some("tool") => {
            json!({"type": "function", "function": {"name": tc.get("name").cloned().unwrap_or(Value::Null)}})
        }
        _ => "auto".into(),
    }
}

fn openai_tool_choice(tc: &Value) -> Value {
    match tc {
        Value::String(s) => match s.as_str() {
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            _ => json!({"type": "auto"}),
        },
        Value::Object(_) => {
            json!({"type": "tool", "name": tc.pointer("/function/name").cloned().unwrap_or(Value::Null)})
        }
        _ => json!({"type": "auto"}),
    }
}

fn openai_finish_to_anth(f: Option<&str>) -> Value {
    match f {
        Some("length") => "max_tokens".into(),
        Some("tool_calls") | Some("function_call") => "tool_use".into(),
        Some("stop") | Some("content_filter") => "end_turn".into(),
        None => Value::Null,
        _ => "end_turn".into(),
    }
}

fn anth_stop_to_openai(s: Option<&str>) -> Value {
    match s {
        Some("max_tokens") => "length".into(),
        Some("tool_use") => "tool_calls".into(),
        Some("end_turn") | Some("stop_sequence") => "stop".into(),
        None => Value::Null,
        _ => "stop".into(),
    }
}

fn anth_event(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    serde_json::to_writer(&mut *out, data).ok();
    out.extend_from_slice(b"\n\n");
}

fn openai_event(out: &mut Vec<u8>, data: &Value) {
    out.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut *out, data).ok();
    out.extend_from_slice(b"\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn request_anthropic_to_openai() {
        let req = json!({
            "model": "gpt-x",
            "max_tokens": 100,
            "system": "be terse",
            "tools": [{"name": "get", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "get", "input": {"q": 1}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "42"}]}
            ]
        });
        let (out, stream) = map_request(Dir::MessagesToChat, req.to_string().as_bytes()).unwrap();
        assert!(!stream);
        let c = v(&out);
        assert_eq!(
            c["messages"][0],
            json!({"role": "system", "content": "be terse"})
        );
        assert_eq!(c["messages"][1], json!({"role": "user", "content": "hi"}));
        assert_eq!(c["messages"][2]["tool_calls"][0]["id"], "t1");
        assert_eq!(c["messages"][2]["tool_calls"][0]["function"]["name"], "get");
        assert_eq!(
            c["messages"][3],
            json!({"role": "tool", "tool_call_id": "t1", "content": "42"})
        );
        assert_eq!(c["tools"][0]["function"]["name"], "get");
        assert_eq!(c["tool_choice"], "required");
    }

    #[test]
    fn request_openai_to_anthropic() {
        let req = json!({
            "model": "claude",
            "stop": "END",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "tool_calls": [{"id": "t1", "function": {"name": "get", "arguments": "{\"q\":1}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "42"}
            ],
            "tools": [{"type": "function", "function": {"name": "get", "parameters": {"type": "object"}}}]
        });
        let (out, _) = map_request(Dir::ChatToMessages, req.to_string().as_bytes()).unwrap();
        let a = v(&out);
        assert_eq!(a["system"], "sys");
        assert_eq!(a["max_tokens"], 4096);
        assert_eq!(a["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(a["messages"][1]["content"][0]["input"], json!({"q": 1}));
        assert_eq!(
            a["messages"][2]["content"][0],
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "42"})
        );
        assert_eq!(a["stop_sequences"], json!(["END"]));
        assert_eq!(a["tools"][0]["name"], "get");
    }

    #[test]
    fn response_openai_to_anthropic() {
        let resp = json!({
            "id": "cc1", "model": "gpt-x",
            "choices": [{"message": {"content": "hello", "tool_calls": [{"id": "t1", "function": {"name": "get", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7}
        });
        let out = v(&map_response(Dir::MessagesToChat, resp.to_string().as_bytes()).unwrap());
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0], json!({"type": "text", "text": "hello"}));
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"], json!({"input_tokens": 5, "output_tokens": 7}));
    }

    #[test]
    fn response_anthropic_to_openai() {
        let resp = json!({
            "id": "m1", "model": "claude",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 4}
        });
        let out = v(&map_response(Dir::ChatToMessages, resp.to_string().as_bytes()).unwrap());
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            out["usage"],
            json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7})
        );
    }

    fn frames(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter_map(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .filter(|d| *d != "[DONE]")
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[test]
    fn stream_openai_to_anthropic() {
        let mut x = SseXlate::new(Dir::MessagesToChat);
        let mut got = x.push(b"data: {\"id\":\"c1\",\"model\":\"gpt-x\",\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n");
        got.extend(x.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n"));
        got.extend(x.push(b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":2}}\n\n"));
        got.extend(x.push(b"data: [DONE]\n\n"));
        let f = frames(&got);
        assert_eq!(f[0]["type"], "message_start");
        assert_eq!(f[1]["type"], "content_block_start");
        assert_eq!(f[2]["delta"]["text"], "He");
        assert_eq!(f[3]["delta"]["text"], "llo");
        let stop = f.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(stop["delta"]["stop_reason"], "end_turn");
        assert_eq!(stop["usage"]["output_tokens"], 2);
        assert!(f.iter().any(|e| e["type"] == "message_stop"));
    }

    #[test]
    fn stream_openai_tool_call_to_anthropic() {
        let mut x = SseXlate::new(Dir::MessagesToChat);
        let mut got = x.push(b"data: {\"id\":\"c1\",\"model\":\"g\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"function\":{\"name\":\"get\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n");
        got.extend(x.push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n"));
        got.extend(x.finish());
        let f = frames(&got);
        let start = f
            .iter()
            .find(|e| e["type"] == "content_block_start")
            .unwrap();
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["name"], "get");
        let deltas: String = f
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .map(|e| e["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "{\"q\":1}");
        let stop = f.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(stop["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn stream_anthropic_to_openai() {
        let mut x = SseXlate::new(Dir::ChatToMessages);
        let mut got = x.push(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude\"}}\n\n");
        got.extend(x.push(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"));
        got.extend(x.push(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n"));
        got.extend(x.push(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"));
        got.extend(x.push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        got.extend(x.finish());
        assert!(got.ends_with(b"data: [DONE]\n\n"));
        let f = frames(&got);
        assert_eq!(f[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(f[1]["choices"][0]["delta"]["content"], "hi");
        let last = f.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
    }
}
