//! The simulated personal world a task runs against.
//!
//! One [`World`] is a JSON document (mail, calendar, contacts, files, money,
//! travel, shopping, dining, messages, reminders, and a small web) plus an
//! append-only log of every service call. The Svit agent reaches it only
//! through host ports ([`WorldPort`]), one per service, so the eval measures
//! personal-assistant work rather than any Svit-specific affordance.
//!
//! Services behave like their real counterparts where it matters for
//! grading: payments check balances and payees, bookings charge a card,
//! returns honour a window, reservations consume a slot, and some outbound
//! mail or texts trigger scripted replies from the people on the other end.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value as Json, json};
use svit::{Port, PortContext, PortDescriptor, PortResult};

/// Shared, mutable world state for one case.
pub struct World {
    inner: Mutex<Inner>,
}

struct Inner {
    state: Json,
    log: Vec<Json>,
    next_id: u64,
}

type Outcome = Result<Json, String>;

impl World {
    pub fn new(state: Json) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                state,
                log: Vec::new(),
                next_id: 1,
            }),
        })
    }

    /// Final world state, for state checks and the judge.
    pub fn state(&self) -> Json {
        self.inner.lock().expect("world lock").state.clone()
    }

    /// Every service call in order.
    pub fn log(&self) -> Vec<Json> {
        self.inner.lock().expect("world lock").log.clone()
    }

    /// Dispatches one service call and records it.
    pub fn call(&self, service: &str, input: Json) -> Outcome {
        let mut inner = self.inner.lock().expect("world lock");
        let action = input
            .get("action")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let result = if input.is_object() || service == "clock" {
            inner.dispatch(service, &action, &input)
        } else {
            Err("input must be a map with an action key, not a string or list".into())
        };
        let seq = inner.log.len() + 1;
        let mut entry = json!({
            "seq": seq,
            "service": service,
            "action": action,
            "input": input,
            "mutating": is_mutating(service, &action),
            "ok": result.is_ok(),
        });
        if let Err(error) = &result {
            entry["error"] = json!(error);
        }
        inner.log.push(entry);
        result
    }
}

fn is_mutating(service: &str, action: &str) -> bool {
    !matches!(
        (service, action),
        ("clock", _)
            | (_, "" | "search" | "list" | "get" | "read" | "fetch" | "now")
            | (
                "bank",
                "accounts" | "transactions" | "payees" | "subscriptions"
            )
            | (
                "travel",
                "search_flights" | "search_hotels" | "list_bookings"
            )
            | ("shop", "list_orders")
            | ("restaurants", "availability" | "list_reservations")
    )
}

impl Inner {
    fn dispatch(&mut self, service: &str, action: &str, input: &Json) -> Outcome {
        match service {
            "clock" => self.clock(),
            "email" => self.email(action, input),
            "calendar" => self.calendar(action, input),
            "contacts" => self.contacts(action, input),
            "files" => self.files(action, input),
            "bank" => self.bank(action, input),
            "travel" => self.travel(action, input),
            "shop" => self.shop(action, input),
            "restaurants" => self.restaurants(action, input),
            "messages" => self.messages(action, input),
            "reminders" => self.reminders(action, input),
            "web" => self.web(action, input),
            _ => Err(format!("unknown service {service}")),
        }
    }

    fn id(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}-{}", 900 + self.next_id);
        self.next_id += 1;
        id
    }

    fn now(&self) -> String {
        self.state["now"]
            .as_str()
            .unwrap_or("2026-11-02T09:00:00-05:00")
            .to_owned()
    }

    fn today(&self) -> String {
        self.now()[..10].to_owned()
    }

    fn coll(&mut self, key: &str) -> &mut Vec<Json> {
        let state = self
            .state
            .as_object_mut()
            .expect("world state is an object");
        let entry = state.entry(key.to_owned()).or_insert_with(|| json!([]));
        if !entry.is_array() {
            *entry = json!([]);
        }
        entry.as_array_mut().expect("array")
    }

    fn items(&self, key: &str) -> Vec<Json> {
        self.state[key].as_array().cloned().unwrap_or_default()
    }

    fn find_mut(&mut self, key: &str, id: &str) -> Result<&mut Json, String> {
        self.coll(key)
            .iter_mut()
            .find(|item| item["id"].as_str() == Some(id))
            .ok_or_else(|| format!("no {key} item with id {id}"))
    }

    // ----- clock -----------------------------------------------------------

    fn clock(&self) -> Outcome {
        let now = self.now();
        Ok(json!({
            "now": now,
            "weekday": weekday(&now[..10]),
            "timezone": self.state["timezone"].clone(),
        }))
    }

    // ----- email -----------------------------------------------------------

    fn email(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search" => {
                let query = str_field(input, "query");
                let folder = str_field(input, "folder");
                let from = str_field(input, "from").to_lowercase();
                let unread_only = input["unread_only"].as_bool().unwrap_or(false);
                let limit = input["limit"].as_u64().unwrap_or(25) as usize;
                let mut found: Vec<Json> = self
                    .items("emails")
                    .into_iter()
                    .filter(|m| {
                        let in_folder = if folder.is_empty() {
                            m["folder"] != "trash"
                        } else {
                            m["folder"].as_str() == Some(folder)
                        };
                        in_folder
                            && (from.is_empty()
                                || m["from"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_lowercase()
                                    .contains(&from))
                            && (!unread_only || !m["read"].as_bool().unwrap_or(false))
                            && text_match(m, query)
                    })
                    .collect();
                found.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
                let total = found.len();
                let results: Vec<Json> = found
                    .into_iter()
                    .take(limit)
                    .map(|m| {
                        json!({
                            "id": m["id"], "folder": m["folder"], "from": m["from"],
                            "to": m["to"], "subject": m["subject"], "date": m["date"],
                            "unread": !m["read"].as_bool().unwrap_or(false),
                            "has_attachments": m["attachments"].as_array().is_some_and(|a| !a.is_empty()),
                            "snippet": snippet(m["body"].as_str().unwrap_or(""), 140),
                        })
                    })
                    .collect();
                Ok(json!({"total": total, "results": results}))
            }
            "read" => {
                let id = required(input, "id")?;
                let message = self.find_mut("emails", &id)?;
                message["read"] = json!(true);
                Ok(message.clone())
            }
            "send" => {
                let to = recipients(&input["to"]);
                if to.is_empty() {
                    return Err("send needs at least one recipient in to".into());
                }
                let subject = required(input, "subject")?;
                let body = required(input, "body")?;
                self.check_attachments(&input["attachments"])?;
                let (id, arrived) = self.send_email(
                    to,
                    recipients(&input["cc"]),
                    subject,
                    body,
                    &input["attachments"],
                    None,
                );
                Ok(sent_result(id, arrived))
            }
            "reply" | "forward" => {
                let id = required(input, "id")?;
                let original = self.find_mut("emails", &id)?.clone();
                let body = str_field(input, "body").to_owned();
                self.check_attachments(&input["attachments"])?;
                let subject = original["subject"].as_str().unwrap_or("");
                let (to, cc, subject, body) = if action == "reply" {
                    let mut to = recipients(&original["from"]);
                    let mut cc = Vec::new();
                    if input["reply_all"].as_bool().unwrap_or(false) {
                        let me = self.state["user"]["email"]
                            .as_str()
                            .unwrap_or("")
                            .to_lowercase();
                        for r in recipients(&original["to"])
                            .into_iter()
                            .chain(recipients(&original["cc"]))
                        {
                            if r.to_lowercase() != me && !to.contains(&r) {
                                cc.push(r);
                            }
                        }
                    }
                    to.dedup();
                    if body.is_empty() {
                        return Err("reply needs a body".into());
                    }
                    (to, cc, prefixed("Re: ", subject), body)
                } else {
                    let to = recipients(&input["to"]);
                    if to.is_empty() {
                        return Err("forward needs to".into());
                    }
                    let quoted = format!(
                        "{body}\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\n\n{}",
                        original["from"].as_str().unwrap_or(""),
                        original["date"].as_str().unwrap_or(""),
                        subject,
                        original["body"].as_str().unwrap_or("")
                    );
                    (
                        to,
                        recipients(&input["cc"]),
                        prefixed("Fwd: ", subject),
                        quoted,
                    )
                };
                let mut attachments = input["attachments"].clone();
                if action == "forward" && original["attachments"].is_array() {
                    let mut all = original["attachments"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    all.extend(attachments.as_array().cloned().unwrap_or_default());
                    attachments = json!(all);
                }
                let (new_id, arrived) =
                    self.send_email(to, cc, subject, body, &attachments, Some(id));
                Ok(sent_result(new_id, arrived))
            }
            "move" => {
                let id = required(input, "id")?;
                let folder = required(input, "folder")?;
                if !["inbox", "archive", "trash", "spam"].contains(&folder.as_str()) {
                    return Err("folder must be inbox, archive, trash, or spam".into());
                }
                let message = self.find_mut("emails", &id)?;
                message["folder"] = json!(folder);
                Ok(json!({"id": id, "folder": folder}))
            }
            "label" => {
                let id = required(input, "id")?;
                let label = required(input, "label")?;
                let message = self.find_mut("emails", &id)?;
                let labels = message
                    .as_object_mut()
                    .expect("email object")
                    .entry("labels")
                    .or_insert_with(|| json!([]));
                if let Some(list) = labels.as_array_mut()
                    && !list.contains(&json!(label))
                {
                    list.push(json!(label));
                }
                Ok(json!({"id": id, "labels": labels.clone()}))
            }
            _ => Err(unknown(
                action,
                "search, read, send, reply, forward, move, label",
            )),
        }
    }

    fn check_attachments(&self, attachments: &Json) -> Result<(), String> {
        let files = self.items("files");
        for path in attachments.as_array().into_iter().flatten() {
            let path = path.as_str().unwrap_or("");
            if !files.iter().any(|f| f["path"].as_str() == Some(path)) {
                return Err(format!("attachment {path} is not in files"));
            }
        }
        Ok(())
    }

    fn send_email(
        &mut self,
        to: Vec<String>,
        cc: Vec<String>,
        subject: String,
        body: String,
        attachments: &Json,
        in_reply_to: Option<String>,
    ) -> (String, Vec<Json>) {
        let id = self.id("mail");
        let from = self.state["user"]["email"].clone();
        let message = json!({
            "id": id, "folder": "sent", "from": from, "to": to, "cc": cc,
            "subject": subject, "body": body, "date": self.now(), "read": true,
            "attachments": attachments.as_array().cloned().unwrap_or_default(),
            "in_reply_to": in_reply_to,
        });
        self.coll("emails").push(message.clone());
        let arrived = self.fire_auto_replies("email", &message);
        (id, arrived)
    }

    /// Scripted replies from the people on the other end. A rule fires once,
    /// when an outbound message reaches its recipient and mentions any of its
    /// keywords. Returns a summary of each arrival, which the send result
    /// surfaces the way a phone notification would.
    fn fire_auto_replies(&mut self, channel: &str, outbound: &Json) -> Vec<Json> {
        let haystack = format!(
            "{} {}",
            outbound["subject"].as_str().unwrap_or(""),
            outbound["body"]
                .as_str()
                .or(outbound["text"].as_str())
                .unwrap_or("")
        )
        .to_lowercase();
        let targets: Vec<String> = match channel {
            "email" => recipients(&outbound["to"])
                .into_iter()
                .chain(recipients(&outbound["cc"]))
                .map(|r| r.to_lowercase())
                .collect(),
            _ => vec![outbound["to"].as_str().unwrap_or("").to_lowercase()],
        };
        let rules = self.items("auto_replies");
        let mut fired = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            if rule["channel"].as_str().unwrap_or("email") != channel
                || rule["fired"].as_bool().unwrap_or(false)
            {
                continue;
            }
            let who = rule["match"]["to"].as_str().unwrap_or("").to_lowercase();
            if who.is_empty() || !targets.iter().any(|t| t.contains(&who)) {
                continue;
            }
            let keywords = string_list(&rule["match"]["any"]);
            if !keywords.is_empty()
                && !keywords
                    .iter()
                    .any(|k| haystack.contains(&k.to_lowercase()))
            {
                continue;
            }
            fired.push((index, rule["reply"].clone()));
        }
        let mut arrived = Vec::new();
        for (index, mut reply) in fired {
            self.coll("auto_replies")[index]["fired"] = json!(true);
            let now = self.now();
            if channel == "email" {
                let id = self.id("mail");
                let me = self.state["user"]["email"].clone();
                let reply_obj = reply.as_object_mut().expect("reply object");
                reply_obj.insert("id".into(), json!(id));
                reply_obj.entry("folder").or_insert(json!("inbox"));
                reply_obj.entry("to").or_insert(json!([me]));
                reply_obj.entry("date").or_insert(json!(now));
                reply_obj.entry("read").or_insert(json!(false));
                reply_obj.entry("subject").or_insert(json!(prefixed(
                    "Re: ",
                    outbound["subject"].as_str().unwrap_or("")
                )));
                arrived.push(
                    json!({"email_id": id, "from": reply["from"], "subject": reply["subject"]}),
                );
                self.coll("emails").push(reply);
            } else {
                let id = self.id("sms");
                let reply_obj = reply.as_object_mut().expect("reply object");
                reply_obj.insert("id".into(), json!(id));
                reply_obj.entry("direction").or_insert(json!("in"));
                reply_obj.entry("date").or_insert(json!(now));
                reply_obj.entry("with").or_insert(outbound["to"].clone());
                arrived.push(json!({"sms_id": id, "from": reply["with"], "text": reply["text"]}));
                self.coll("messages").push(reply);
            }
        }
        arrived
    }

    // ----- calendar --------------------------------------------------------

    fn calendar(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "list" => {
                let from = str_field(input, "from");
                let to = str_field(input, "to");
                let query = str_field(input, "query");
                let include_cancelled = input["include_cancelled"].as_bool().unwrap_or(false);
                let mut events: Vec<Json> = self
                    .items("calendar")
                    .into_iter()
                    .filter(|e| {
                        let start = e["start"].as_str().unwrap_or("");
                        let end = e["end"].as_str().unwrap_or(start);
                        (include_cancelled || e["status"] != "cancelled")
                            && (from.is_empty() || end >= from)
                            && (to.is_empty() || start <= to || start.starts_with(to))
                            && text_match(e, query)
                    })
                    .collect();
                events.sort_by(|a, b| a["start"].as_str().cmp(&b["start"].as_str()));
                Ok(json!({"events": events}))
            }
            "create" => {
                let title = required(input, "title")?;
                let start = self.instant(&required(input, "start")?)?;
                let end = self.instant(&required(input, "end")?)?;
                if end <= start {
                    return Err("end must be after start".into());
                }
                let overlaps = self.overlaps(&start, &end, None);
                let id = self.id("evt");
                let event = json!({
                    "id": id, "title": title, "start": start, "end": end,
                    "attendees": recipients(&input["attendees"]),
                    "location": input["location"].clone(),
                    "notes": input["notes"].clone(),
                    "status": "confirmed",
                });
                self.coll("calendar").push(event.clone());
                let arrived = self.notify_attendees("Invitation", &event);
                Ok(with_arrivals(
                    json!({"id": id, "overlaps": overlaps}),
                    arrived,
                ))
            }
            "update" => {
                let id = required(input, "id")?;
                let mut fields = input.clone();
                for key in ["start", "end"] {
                    if let Some(value) = input[key].as_str() {
                        fields[key] = json!(self.instant(value)?);
                    }
                }
                let event = self.find_mut("calendar", &id)?;
                for key in ["title", "start", "end", "location", "notes", "attendees"] {
                    if let Some(value) = fields.get(key) {
                        event[key] = if key == "attendees" {
                            json!(recipients(value))
                        } else {
                            value.clone()
                        };
                    }
                }
                let event = event.clone();
                let (start, end) = (
                    event["start"].as_str().unwrap_or("").to_owned(),
                    event["end"].as_str().unwrap_or("").to_owned(),
                );
                let overlaps = self.overlaps(&start, &end, Some(&id));
                let arrived = self.notify_attendees("Updated invitation", &event);
                Ok(with_arrivals(
                    json!({"id": id, "overlaps": overlaps}),
                    arrived,
                ))
            }
            "cancel" => {
                let id = required(input, "id")?;
                let event = self.find_mut("calendar", &id)?;
                event["status"] = json!("cancelled");
                let event = event.clone();
                let arrived = self.notify_attendees("Cancelled", &event);
                Ok(with_arrivals(
                    json!({"id": id, "status": "cancelled"}),
                    arrived,
                ))
            }
            _ => Err(unknown(action, "list, create, update, cancel")),
        }
    }

    /// Mails every attendee other than the user, as a calendar service sends
    /// invitations. The invite shows times only in the organizer's zone, so
    /// telling someone their local time still takes a written message.
    fn notify_attendees(&mut self, kind: &str, event: &Json) -> Vec<Json> {
        let me = self.state["user"]["email"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        let to: Vec<String> = recipients(&event["attendees"])
            .into_iter()
            .filter(|a| a.to_lowercase() != me)
            .collect();
        if to.is_empty() {
            return Vec::new();
        }
        let title = event["title"].as_str().unwrap_or("");
        let body = format!(
            "{kind}: {title}\nWhen: {} to {}\nWhere: {}\n\n{}",
            event["start"].as_str().unwrap_or(""),
            event["end"].as_str().unwrap_or(""),
            event["location"].as_str().unwrap_or("(none)"),
            event["notes"].as_str().unwrap_or(""),
        );
        let (_, arrived) = self.send_email(
            to,
            Vec::new(),
            format!("{kind}: {title}"),
            body,
            &json!([]),
            None,
        );
        arrived
    }

    /// Normalizes an ISO 8601 time to the user's UTC offset, as a calendar
    /// stores instants and shows them in the owner's zone.
    fn instant(&self, value: &str) -> Result<String, String> {
        let offset = self.state["utc_offset_minutes"].as_i64().unwrap_or(-300);
        parse_instant(value, offset)
            .map(|minutes| format_instant(minutes, offset))
            .ok_or_else(|| {
                format!("{value} is not an ISO 8601 time like 2026-11-03T08:00:00-05:00")
            })
    }

    fn overlaps(&self, start: &str, end: &str, skip: Option<&str>) -> Vec<Json> {
        self.items("calendar")
            .into_iter()
            .filter(|e| {
                e["status"] != "cancelled"
                    && skip != e["id"].as_str()
                    && e["start"].as_str().unwrap_or("") < end
                    && e["end"].as_str().unwrap_or("") > start
            })
            .map(|e| json!({"id": e["id"], "title": e["title"], "start": e["start"], "end": e["end"]}))
            .collect()
    }

    // ----- contacts --------------------------------------------------------

    fn contacts(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search" => {
                let query = str_field(input, "query");
                let found: Vec<Json> = self
                    .items("contacts")
                    .into_iter()
                    .filter(|c| text_match(c, query))
                    .collect();
                Ok(json!({"results": found}))
            }
            "get" => {
                let id = required(input, "id")?;
                Ok(self.find_mut("contacts", &id)?.clone())
            }
            "create" => {
                let name = required(input, "name")?;
                let id = self.id("ct");
                let mut contact = input.clone();
                contact["id"] = json!(id);
                contact["name"] = json!(name);
                if let Some(obj) = contact.as_object_mut() {
                    obj.remove("action");
                }
                self.coll("contacts").push(contact);
                Ok(json!({"id": id}))
            }
            "update" => {
                let id = required(input, "id")?;
                let contact = self.find_mut("contacts", &id)?;
                for (key, value) in input.as_object().into_iter().flatten() {
                    if key != "action" && key != "id" {
                        contact[key] = value.clone();
                    }
                }
                Ok(contact.clone())
            }
            _ => Err(unknown(action, "search, get, create, update")),
        }
    }

    // ----- files -----------------------------------------------------------

    fn files(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "list" => {
                let prefix = str_field(input, "prefix");
                let listing: Vec<Json> = self
                    .items("files")
                    .into_iter()
                    .filter(|f| f["path"].as_str().unwrap_or("").starts_with(prefix))
                    .map(|f| json!({"path": f["path"], "bytes": f["content"].as_str().map_or(0, str::len)}))
                    .collect();
                Ok(json!({"files": listing}))
            }
            "search" => {
                let query = str_field(input, "query");
                let listing: Vec<Json> = self
                    .items("files")
                    .into_iter()
                    .filter(|f| text_match(f, query))
                    .map(|f| json!({"path": f["path"], "snippet": snippet(f["content"].as_str().unwrap_or(""), 140)}))
                    .collect();
                Ok(json!({"files": listing}))
            }
            "read" => {
                let path = required(input, "path")?;
                self.items("files")
                    .into_iter()
                    .find(|f| f["path"].as_str() == Some(path.as_str()))
                    .ok_or_else(|| format!("no file at {path}"))
            }
            "write" => {
                let path = required(input, "path")?;
                let content = required(input, "content")?;
                let now = self.now();
                let files = self.coll("files");
                if let Some(file) = files
                    .iter_mut()
                    .find(|f| f["path"].as_str() == Some(path.as_str()))
                {
                    file["content"] = json!(content);
                    file["modified"] = json!(now);
                } else {
                    files.push(json!({"path": path, "content": content, "modified": now}));
                }
                Ok(json!({"path": path, "bytes": content.len()}))
            }
            "delete" => {
                let path = required(input, "path")?;
                let files = self.coll("files");
                let before = files.len();
                files.retain(|f| f["path"].as_str() != Some(path.as_str()));
                if files.len() == before {
                    return Err(format!("no file at {path}"));
                }
                Ok(json!({"deleted": path}))
            }
            _ => Err(unknown(action, "list, search, read, write, delete")),
        }
    }

    // ----- bank ------------------------------------------------------------

    fn bank(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "accounts" => Ok(json!({"accounts": self.items("accounts")})),
            "payees" => Ok(json!({"payees": self.items("payees")})),
            "subscriptions" => Ok(json!({"subscriptions": self.items("subscriptions")})),
            "transactions" => {
                let account = str_field(input, "account");
                let from = str_field(input, "from");
                let to = str_field(input, "to");
                let query = str_field(input, "query");
                let mut found: Vec<Json> = self
                    .items("transactions")
                    .into_iter()
                    .filter(|t| {
                        let date = t["date"].as_str().unwrap_or("");
                        (account.is_empty() || t["account"].as_str() == Some(account))
                            && (from.is_empty() || date >= from)
                            && (to.is_empty() || date <= to)
                            && text_match(t, query)
                    })
                    .collect();
                found.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str()));
                Ok(json!({"transactions": found}))
            }
            "add_payee" => {
                let name = required(input, "name")?;
                let account_number = required(input, "account_number")?;
                let id = self.id("payee");
                let added = self.now();
                self.coll("payees").push(json!({
                    "id": id, "name": name, "account_number": account_number,
                    "bank": input["bank"].clone(), "added": added,
                }));
                Ok(json!({"id": id}))
            }
            "transfer" => {
                let from_account = required(input, "from_account")?;
                let payee_ref = required(input, "payee")?.to_lowercase();
                let amount = input["amount"]
                    .as_f64()
                    .filter(|a| *a > 0.0)
                    .ok_or("amount must be a positive number")?;
                let payee = self
                    .items("payees")
                    .into_iter()
                    .find(|p| {
                        p["id"].as_str().map(str::to_lowercase).as_deref()
                            == Some(payee_ref.as_str())
                            || p["name"].as_str().map(str::to_lowercase).as_deref()
                                == Some(payee_ref.as_str())
                    })
                    .ok_or("unknown payee; list payees or add_payee first")?;
                let date = input["date"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| self.today());
                let scheduled = date > self.today();
                let account = self.find_mut("accounts", &from_account)?;
                let balance = account["balance"].as_f64().unwrap_or(0.0);
                if account["type"] != "credit" && balance < amount {
                    return Err(format!("insufficient funds: balance {balance:.2}"));
                }
                account["balance"] = json!(round2(balance - amount));
                if let Some(internal) = payee["internal_account"].as_str()
                    && let Ok(target) = self.find_mut("accounts", internal)
                {
                    let balance = target["balance"].as_f64().unwrap_or(0.0);
                    target["balance"] = json!(round2(balance + amount));
                }
                let id = self.id("tx");
                let memo = input["memo"].clone();
                self.coll("transactions").push(json!({
                    "id": id, "account": from_account, "date": date,
                    "merchant": payee["name"], "amount": -amount, "category": "transfer",
                    "memo": memo, "status": if scheduled { "scheduled" } else { "posted" },
                }));
                self.coll("transfers").push(json!({
                    "id": id, "from_account": from_account, "payee": payee["name"],
                    "payee_id": payee["id"], "amount": amount, "memo": memo, "date": date,
                }));
                Ok(json!({"id": id, "status": if scheduled { "scheduled" } else { "posted" }}))
            }
            "dispute" => {
                let id = required(input, "transaction_id")?;
                let reason = required(input, "reason")?;
                let tx = self.find_mut("transactions", &id)?;
                tx["status"] = json!("disputed");
                tx["dispute_reason"] = json!(reason);
                Ok(json!({"id": id, "status": "disputed"}))
            }
            "cancel_subscription" => {
                let id = required(input, "id")?;
                let sub = self.find_mut("subscriptions", &id)?;
                sub["status"] = json!("cancelled");
                Ok(json!({"id": id, "status": "cancelled"}))
            }
            _ => Err(unknown(
                action,
                "accounts, transactions, payees, add_payee, transfer, dispute, subscriptions, cancel_subscription",
            )),
        }
    }

    fn charge(
        &mut self,
        account: Option<&str>,
        amount: f64,
        merchant: &str,
    ) -> Result<String, String> {
        let account_id = match account {
            Some(id) => id.to_owned(),
            None => self.state["default_card"]
                .as_str()
                .unwrap_or("acc-card")
                .to_owned(),
        };
        let date = self.today();
        let acct = self.find_mut("accounts", &account_id)?;
        let balance = acct["balance"].as_f64().unwrap_or(0.0);
        if acct["type"] == "credit" {
            let limit = acct["credit_limit"].as_f64().unwrap_or(f64::MAX);
            if -balance + amount > limit {
                return Err("card declined: credit limit".into());
            }
        } else if balance < amount {
            return Err("payment declined: insufficient funds".into());
        }
        acct["balance"] = json!(round2(balance - amount));
        let id = self.id("tx");
        self.coll("transactions").push(json!({
            "id": id, "account": account_id, "date": date, "merchant": merchant,
            "amount": -round2(amount), "category": "purchase", "status": "posted",
        }));
        Ok(account_id)
    }

    // ----- travel ----------------------------------------------------------

    fn travel(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search_flights" => {
                let from = str_field(input, "from").to_lowercase();
                let to = str_field(input, "to").to_lowercase();
                let date = str_field(input, "date");
                let found: Vec<Json> = self
                    .items("flights")
                    .into_iter()
                    .filter(|f| {
                        place_match(&f["from"], &from)
                            && place_match(&f["to"], &to)
                            && (date.is_empty()
                                || f["depart"].as_str().unwrap_or("").starts_with(date))
                    })
                    .collect();
                Ok(json!({"flights": found}))
            }
            "search_hotels" => {
                let city = str_field(input, "city").to_lowercase();
                let max_price = input["max_price"].as_f64();
                let found: Vec<Json> = self
                    .items("hotels")
                    .into_iter()
                    .filter(|h| {
                        place_match(&h["city"], &city)
                            && max_price
                                .is_none_or(|m| h["price_per_night"].as_f64().unwrap_or(0.0) <= m)
                    })
                    .collect();
                Ok(json!({"hotels": found}))
            }
            "book_flight" => {
                let flight_id = required(input, "flight_id")?;
                let passengers = string_list(&input["passengers"]);
                if passengers.is_empty() {
                    return Err("passengers must list traveller names".into());
                }
                let flight = self.find_mut("flights", &flight_id)?.clone();
                let seats = flight["seats_left"].as_u64().unwrap_or(9);
                if (passengers.len() as u64) > seats {
                    return Err(format!("only {seats} seats left"));
                }
                let total = flight["price"].as_f64().unwrap_or(0.0) * passengers.len() as f64;
                let merchant = flight["airline"].as_str().unwrap_or("Airline").to_owned();
                let account = self.charge(input["payment_account"].as_str(), total, &merchant)?;
                self.find_mut("flights", &flight_id)?["seats_left"] =
                    json!(seats - passengers.len() as u64);
                let id = self.id("bk");
                self.coll("bookings").push(json!({
                    "id": id, "kind": "flight", "flight_id": flight_id, "passengers": passengers,
                    "total": round2(total), "account": account, "status": "confirmed",
                    "refundable": flight["refundable"].clone(), "depart": flight["depart"].clone(),
                }));
                Ok(json!({"booking_id": id, "total": round2(total), "status": "confirmed"}))
            }
            "book_hotel" => {
                let hotel_id = required(input, "hotel_id")?;
                let check_in = required(input, "check_in")?;
                let check_out = required(input, "check_out")?;
                let nights = day_number(&check_out) - day_number(&check_in);
                if nights <= 0 {
                    return Err("check_out must be after check_in".into());
                }
                let hotel = self.find_mut("hotels", &hotel_id)?.clone();
                for blocked in string_list(&hotel["unavailable_nights"]) {
                    if blocked.as_str() >= check_in.as_str()
                        && blocked.as_str() < check_out.as_str()
                    {
                        return Err(format!("no availability on {blocked}"));
                    }
                }
                let rate_code = str_field(input, "rate_code").to_uppercase();
                let nightly = if rate_code.is_empty() {
                    hotel["price_per_night"].as_f64().unwrap_or(0.0)
                } else {
                    hotel["rate_codes"][&rate_code].as_f64().ok_or_else(|| {
                        format!("rate code {rate_code} is not valid for this hotel")
                    })?
                };
                let total = nightly * nights as f64;
                let merchant = hotel["name"].as_str().unwrap_or("Hotel").to_owned();
                let account = self.charge(input["payment_account"].as_str(), total, &merchant)?;
                let id = self.id("bk");
                self.coll("bookings").push(json!({
                    "id": id, "kind": "hotel", "hotel_id": hotel_id, "check_in": check_in,
                    "check_out": check_out, "guests": input["guests"].clone(), "nights": nights,
                    "rate_code": if rate_code.is_empty() { Json::Null } else { json!(rate_code) },
                    "total": round2(total), "account": account, "status": "confirmed",
                    "refundable": hotel["refundable"].clone(),
                }));
                Ok(json!({"booking_id": id, "total": round2(total), "status": "confirmed"}))
            }
            "list_bookings" => Ok(json!({"bookings": self.items("bookings")})),
            "cancel_booking" => {
                let id = required(input, "booking_id")?;
                let booking = self.find_mut("bookings", &id)?;
                if booking["status"] == "cancelled" {
                    return Err("booking already cancelled".into());
                }
                booking["status"] = json!("cancelled");
                let refundable = booking["refundable"].as_bool().unwrap_or(false);
                let total = booking["total"].as_f64().unwrap_or(0.0);
                let account = booking["account"].as_str().unwrap_or("").to_owned();
                let refund = if refundable { total } else { 0.0 };
                if refund > 0.0
                    && let Ok(acct) = self.find_mut("accounts", &account)
                {
                    let balance = acct["balance"].as_f64().unwrap_or(0.0);
                    acct["balance"] = json!(round2(balance + refund));
                }
                Ok(json!({"booking_id": id, "status": "cancelled", "refund": refund}))
            }
            _ => Err(unknown(
                action,
                "search_flights, search_hotels, book_flight, book_hotel, list_bookings, cancel_booking",
            )),
        }
    }

    // ----- shop ------------------------------------------------------------

    fn shop(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search" => {
                let query = str_field(input, "query");
                let max_price = input["max_price"].as_f64();
                let found: Vec<Json> = self
                    .items("products")
                    .into_iter()
                    .filter(|p| {
                        text_match(p, query)
                            && max_price.is_none_or(|m| p["price"].as_f64().unwrap_or(0.0) <= m)
                    })
                    .collect();
                Ok(json!({"products": found}))
            }
            "get" => {
                let id = required(input, "product_id")?;
                Ok(self.find_mut("products", &id)?.clone())
            }
            "order" => {
                let id = required(input, "product_id")?;
                let quantity = input["quantity"].as_u64().unwrap_or(1).max(1);
                let product = self.find_mut("products", &id)?.clone();
                let stock = product["stock"].as_u64().unwrap_or(10);
                if quantity > stock {
                    return Err(format!("only {stock} in stock"));
                }
                let total = product["price"].as_f64().unwrap_or(0.0) * quantity as f64;
                let account = self.charge(input["payment_account"].as_str(), total, "Shop")?;
                self.find_mut("products", &id)?["stock"] = json!(stock - quantity);
                let order_id = self.id("ord");
                let delivery = add_days(
                    &self.today(),
                    product["delivery_days"].as_i64().unwrap_or(3),
                );
                let address = input
                    .get("shipping_address")
                    .cloned()
                    .unwrap_or_else(|| self.state["user"]["address"].clone());
                let today = self.today();
                self.coll("orders").push(json!({
                    "id": order_id, "product_id": id, "name": product["name"], "quantity": quantity,
                    "total": round2(total), "account": account, "date": today,
                    "estimated_delivery": delivery, "shipping_address": address,
                    "gift_message": input["gift_message"].clone(), "status": "placed",
                    "return_window_days": product["return_window_days"].clone(),
                }));
                Ok(
                    json!({"order_id": order_id, "total": round2(total), "estimated_delivery": delivery}),
                )
            }
            "list_orders" => Ok(json!({"orders": self.items("orders")})),
            "cancel_order" => {
                let id = required(input, "order_id")?;
                let order = self.find_mut("orders", &id)?;
                if order["status"] != "placed" {
                    return Err(format!(
                        "order is {}; only placed orders can be cancelled",
                        order["status"]
                    ));
                }
                order["status"] = json!("cancelled");
                Ok(json!({"order_id": id, "status": "cancelled"}))
            }
            "return_order" => {
                let id = required(input, "order_id")?;
                let reason = required(input, "reason")?;
                let today = day_number(&self.today());
                let order = self.find_mut("orders", &id)?;
                if order["status"] != "delivered" {
                    return Err("only delivered orders can be returned".into());
                }
                let defective = [
                    "defect",
                    "broken",
                    "damaged",
                    "faulty",
                    "not working",
                    "doesn't work",
                ]
                .iter()
                .any(|w| reason.to_lowercase().contains(w));
                let window = if defective {
                    order["defect_window_days"].as_i64().unwrap_or(30)
                } else {
                    order["return_window_days"].as_i64().unwrap_or(30)
                };
                let delivered = order["delivered_on"]
                    .as_str()
                    .unwrap_or("2000-01-01")
                    .to_owned();
                if today - day_number(&delivered) > window {
                    return Err(format!("return window of {window} days has closed"));
                }
                order["status"] = json!("return_requested");
                order["return_reason"] = json!(reason);
                Ok(json!({"order_id": id, "status": "return_requested"}))
            }
            _ => Err(unknown(
                action,
                "search, get, order, list_orders, cancel_order, return_order",
            )),
        }
    }

    // ----- restaurants -----------------------------------------------------

    fn restaurants(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search" => {
                let query = str_field(input, "query");
                let city = str_field(input, "city").to_lowercase();
                let found: Vec<Json> = self
                    .items("restaurants")
                    .into_iter()
                    .filter(|r| text_match(r, query) && place_match(&r["city"], &city))
                    .map(|mut r| {
                        if let Some(obj) = r.as_object_mut() {
                            obj.remove("slots");
                        }
                        r
                    })
                    .collect();
                Ok(json!({"restaurants": found}))
            }
            "availability" => {
                let id = required(input, "restaurant_id")?;
                let date = required(input, "date")?;
                let restaurant = self.find_mut("restaurants", &id)?;
                Ok(
                    json!({"restaurant_id": id, "date": date, "slots": restaurant["slots"][&date].clone()}),
                )
            }
            "reserve" => {
                let id = required(input, "restaurant_id")?;
                let date = required(input, "date")?;
                let time = required(input, "time")?;
                let party = input["party_size"]
                    .as_u64()
                    .ok_or("party_size is required")?;
                let restaurant = self.find_mut("restaurants", &id)?;
                let name = restaurant["name"].clone();
                let slots = restaurant["slots"][&date]
                    .as_array_mut()
                    .ok_or_else(|| format!("no availability on {date}"))?;
                let index = slots
                    .iter()
                    .position(|s| s["time"].as_str() == Some(time.as_str()))
                    .ok_or_else(|| format!("{time} is not available on {date}"))?;
                if slots[index]["max_party"].as_u64().unwrap_or(0) < party {
                    return Err(format!(
                        "the {time} table seats at most {}",
                        slots[index]["max_party"]
                    ));
                }
                slots.remove(index);
                let reservation_id = self.id("rsv");
                self.coll("reservations").push(json!({
                    "id": reservation_id, "restaurant_id": id, "restaurant": name, "date": date,
                    "time": time, "party_size": party, "name": input["name"].clone(),
                    "notes": input["notes"].clone(), "status": "confirmed",
                }));
                Ok(json!({"reservation_id": reservation_id, "status": "confirmed"}))
            }
            "list_reservations" => Ok(json!({"reservations": self.items("reservations")})),
            "cancel_reservation" => {
                let id = required(input, "reservation_id")?;
                let reservation = self.find_mut("reservations", &id)?;
                reservation["status"] = json!("cancelled");
                Ok(json!({"reservation_id": id, "status": "cancelled"}))
            }
            _ => Err(unknown(
                action,
                "search, availability, reserve, list_reservations, cancel_reservation",
            )),
        }
    }

    // ----- messages --------------------------------------------------------

    fn messages(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "list" => {
                let with = str_field(input, "with").to_lowercase();
                let found: Vec<Json> = self
                    .items("messages")
                    .into_iter()
                    .filter(|m| {
                        with.is_empty()
                            || m["with"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&with)
                    })
                    .collect();
                Ok(json!({"messages": found}))
            }
            "send" => {
                let to = required(input, "to")?;
                let text = required(input, "text")?;
                let resolved = self
                    .items("contacts")
                    .into_iter()
                    .find(|c| {
                        c["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&to))
                    })
                    .and_then(|c| c["phone"].as_str().map(str::to_owned))
                    .unwrap_or(to.clone());
                let id = self.id("sms");
                let message = json!({
                    "id": id, "with": resolved, "to": resolved, "direction": "out",
                    "text": text, "date": self.now(),
                });
                self.coll("messages").push(message.clone());
                let arrived = self.fire_auto_replies("sms", &message);
                let mut result = json!({"id": id, "to": resolved, "status": "delivered"});
                if !arrived.is_empty() {
                    result["new_messages"] = json!(arrived);
                }
                Ok(result)
            }
            _ => Err(unknown(action, "list, send")),
        }
    }

    // ----- reminders -------------------------------------------------------

    fn reminders(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "list" => Ok(json!({"reminders": self.items("reminders")})),
            "create" => {
                let text = required(input, "text")?;
                let id = self.id("rem");
                self.coll("reminders").push(json!({
                    "id": id, "text": text, "due": input["due"].clone(), "done": false,
                }));
                Ok(json!({"id": id}))
            }
            "complete" => {
                let id = required(input, "id")?;
                self.find_mut("reminders", &id)?["done"] = json!(true);
                Ok(json!({"id": id, "done": true}))
            }
            "delete" => {
                let id = required(input, "id")?;
                let list = self.coll("reminders");
                let before = list.len();
                list.retain(|r| r["id"].as_str() != Some(id.as_str()));
                if list.len() == before {
                    return Err(format!("no reminder {id}"));
                }
                Ok(json!({"deleted": id}))
            }
            _ => Err(unknown(action, "list, create, complete, delete")),
        }
    }

    // ----- web -------------------------------------------------------------

    fn web(&mut self, action: &str, input: &Json) -> Outcome {
        match action {
            "search" => {
                let query = required(input, "query")?.to_lowercase();
                let terms: Vec<&str> = query.split_whitespace().filter(|t| t.len() > 2).collect();
                let mut scored: Vec<(usize, Json)> = self
                    .items("web")
                    .into_iter()
                    .map(|page| {
                        let text = format!(
                            "{} {}",
                            page["title"].as_str().unwrap_or(""),
                            page["content"].as_str().unwrap_or("")
                        )
                        .to_lowercase();
                        (terms.iter().filter(|t| text.contains(**t)).count(), page)
                    })
                    .filter(|(score, _)| *score > 0)
                    .collect();
                scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
                let results: Vec<Json> = scored
                    .into_iter()
                    .take(5)
                    .map(|(_, p)| json!({"url": p["url"], "title": p["title"], "snippet": snippet(p["content"].as_str().unwrap_or(""), 160)}))
                    .collect();
                Ok(json!({"results": results}))
            }
            "fetch" => {
                let url = required(input, "url")?;
                self.items("web")
                    .into_iter()
                    .find(|p| p["url"].as_str() == Some(url.as_str()))
                    .ok_or_else(|| format!("404: {url}"))
            }
            _ => Err(unknown(action, "search, fetch")),
        }
    }
}

// ----- helpers ---------------------------------------------------------------

fn unknown(action: &str, supported: &str) -> String {
    format!("unknown action '{action}'; supported: {supported}")
}

fn str_field<'a>(input: &'a Json, key: &str) -> &'a str {
    input[key].as_str().unwrap_or("")
}

fn required(input: &Json, key: &str) -> Result<String, String> {
    match &input[key] {
        Json::String(s) if !s.trim().is_empty() => Ok(s.clone()),
        Json::Number(n) => Ok(n.to_string()),
        _ => Err(format!("{key} is required")),
    }
}

fn recipients(value: &Json) -> Vec<String> {
    match value {
        Json::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Json::Array(items) => items.iter().flat_map(recipients).collect(),
        _ => Vec::new(),
    }
}

pub fn string_list(value: &Json) -> Vec<String> {
    match value {
        Json::String(s) => vec![s.clone()],
        Json::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn prefixed(prefix: &str, subject: &str) -> String {
    if subject.to_lowercase().starts_with(&prefix.to_lowercase()) {
        subject.to_owned()
    } else {
        format!("{prefix}{subject}")
    }
}

fn snippet(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    }
}

/// Every whitespace-separated query term occurs somewhere in the item.
fn text_match(item: &Json, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let haystack = item.to_string().to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|term| haystack.contains(term))
}

fn place_match(field: &Json, wanted: &str) -> bool {
    wanted.is_empty() || field.to_string().to_lowercase().contains(wanted)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` prefix (proleptic Gregorian).
pub fn day_number(date: &str) -> i64 {
    let parse = |range: std::ops::Range<usize>| date.get(range).and_then(|s| s.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d)) = (parse(0..4), parse(5..7), parse(8..10)) else {
        return 0;
    };
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn add_days(date: &str, days: i64) -> String {
    let z = day_number(date) + days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Minutes since the epoch for `YYYY-MM-DDTHH:MM[:SS][.f][Z|±HH:MM]`; a time
/// without an offset is read in `default_offset` minutes from UTC.
pub fn parse_instant(value: &str, default_offset: i64) -> Option<i64> {
    let re = regex::Regex::new(
        r"^(\d{4}-\d{2}-\d{2})[T ](\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?\s*(Z|[+-]\d{2}:?\d{2})?$",
    )
    .expect("valid instant pattern");
    let caps = re.captures(value.trim())?;
    let date = caps.get(1)?.as_str();
    let hours: i64 = caps.get(2)?.as_str().parse().ok()?;
    let minutes: i64 = caps.get(3)?.as_str().parse().ok()?;
    if hours > 23 || minutes > 59 || day_number(date) == 0 {
        return None;
    }
    let offset = match caps.get(5).map(|m| m.as_str()) {
        None => default_offset,
        Some("Z") => 0,
        Some(text) => {
            let sign = if text.starts_with('-') { -1 } else { 1 };
            let digits: String = text[1..].chars().filter(char::is_ascii_digit).collect();
            let h: i64 = digits.get(0..2)?.parse().ok()?;
            let m: i64 = digits.get(2..4)?.parse().ok()?;
            sign * (h * 60 + m)
        }
    };
    Some(day_number(date) * 1440 + hours * 60 + minutes - offset)
}

pub fn format_instant(minutes: i64, offset: i64) -> String {
    let local = minutes + offset;
    let date = add_days("1970-01-01", local.div_euclid(1440));
    let of_day = local.rem_euclid(1440);
    let sign = if offset < 0 { '-' } else { '+' };
    format!(
        "{date}T{:02}:{:02}:00{sign}{:02}:{:02}",
        of_day / 60,
        of_day % 60,
        offset.abs() / 60,
        offset.abs() % 60
    )
}

fn weekday(date: &str) -> &'static str {
    const DAYS: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    DAYS[day_number(date).rem_euclid(7) as usize]
}

// ----- ports -----------------------------------------------------------------

/// One world service exposed to the Svit agent as a host port.
pub struct WorldPort {
    world: Arc<World>,
    service: &'static str,
}

/// Service names and the action reference shown under `/ports/<name>`.
pub const SERVICES: &[(&str, &str)] = &[
    (
        "clock",
        "Current local date and time for the user. Input: {}.",
    ),
    (
        "email",
        "The user's mailbox. Actions: search {query?, folder? (inbox|sent|archive|spam|trash), from?, unread_only?, limit?}; read {id}; send {to: [addr], cc?: [addr], subject, body, attachments?: [file path]}; reply {id, body, reply_all?, attachments?}; forward {id, to: [addr], body?}; move {id, folder}; label {id, label}. Sending is immediate and cannot be undone. A send result lists any replies that arrive in response under new_messages.",
    ),
    (
        "calendar",
        "The user's calendar. Times are ISO 8601 with offset. Actions: list {from?, to?, query?, include_cancelled?}; create {title, start, end, attendees?: [addr], location?, notes?} (reports overlaps); update {id, title?, start?, end?, location?, notes?, attendees?}; cancel {id}. Create, update, and cancel email every attendee an invitation showing times in the user's zone; replies arrive under new_messages.",
    ),
    (
        "contacts",
        "The user's address book. Actions: search {query}; get {id}; create {name, email?, phone?, notes?, ...}; update {id, ...fields}.",
    ),
    (
        "files",
        "The user's documents. Actions: list {prefix?}; search {query}; read {path}; write {path, content}; delete {path}.",
    ),
    (
        "bank",
        "The user's bank and cards. Amounts are in account currency; negative transaction amounts are debits. Actions: accounts {}; transactions {account?, from? (YYYY-MM-DD), to?, query?}; payees {}; add_payee {name, account_number, bank?}; transfer {from_account, payee (id or exact name), amount, memo?, date? (future date schedules it)}; dispute {transaction_id, reason}; subscriptions {}; cancel_subscription {id}. Transfers move real money and cannot be undone.",
    ),
    (
        "travel",
        "Flights and hotels. Actions: search_flights {from, to, date (YYYY-MM-DD)}; search_hotels {city, max_price?}; book_flight {flight_id, passengers: [full name], payment_account?}; book_hotel {hotel_id, check_in, check_out, guests, rate_code?, payment_account?}; list_bookings {}; cancel_booking {booking_id} (non-refundable bookings refund nothing). Bookings charge the default card unless payment_account is given.",
    ),
    (
        "shop",
        "Online store. Actions: search {query, max_price?}; get {product_id}; order {product_id, quantity?, shipping_address?, gift_message?, payment_account?}; list_orders {}; cancel_order {order_id}; return_order {order_id, reason}. Orders charge the default card.",
    ),
    (
        "restaurants",
        "Restaurant search and booking. Actions: search {query?, city?}; availability {restaurant_id, date}; reserve {restaurant_id, date, time, party_size, name?, notes?}; list_reservations {}; cancel_reservation {reservation_id}.",
    ),
    (
        "messages",
        "SMS on the user's phone. Actions: list {with?}; send {to (contact name or phone), text}. Sending is immediate; replies that arrive in response are listed under new_messages.",
    ),
    (
        "reminders",
        "The user's reminders. Actions: list {}; create {text, due?}; complete {id}; delete {id}.",
    ),
    ("web", "Web search. Actions: search {query}; fetch {url}."),
];

impl WorldPort {
    pub fn all(world: &Arc<World>) -> Vec<(String, Box<dyn Port>)> {
        SERVICES
            .iter()
            .map(|(service, _)| {
                (
                    (*service).to_owned(),
                    Box::new(WorldPort {
                        world: world.clone(),
                        service,
                    }) as Box<dyn Port>,
                )
            })
            .collect()
    }
}

#[svit::async_trait]
impl Port for WorldPort {
    fn descriptor(&self) -> PortDescriptor {
        let description = SERVICES
            .iter()
            .find(|(name, _)| *name == self.service)
            .map(|(_, d)| *d)
            .unwrap_or("");
        let mut properties = Map::new();
        properties.insert(
            "action".into(),
            json!({"type": "string", "description": "Operation to perform; see the description."}),
        );
        PortDescriptor::new(
            description,
            json!({"type": "object", "properties": properties, "additionalProperties": true}),
        )
        .effect(if self.service == "clock" || self.service == "web" {
            "read"
        } else {
            "external"
        })
        .output("JSON result of the action, or an error message.")
    }

    async fn execute(&self, _context: PortContext, input: Json) -> PortResult {
        match self.world.call(self.service, input) {
            Ok(value) => PortResult::success(value),
            Err(error) => PortResult::error(error),
        }
    }
}

/// A send result, plus any replies that arrived because of it.
fn sent_result(id: String, arrived: Vec<Json>) -> Json {
    with_arrivals(json!({"id": id, "status": "sent"}), arrived)
}

/// Adds the replies an action caused to its result, as `new_messages`.
fn with_arrivals(mut result: Json, arrived: Vec<Json>) -> Json {
    if !arrived.is_empty() {
        result["new_messages"] = json!(arrived);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_arithmetic_round_trips() {
        assert_eq!(weekday("2026-11-02"), "Monday");
        assert_eq!(add_days("2026-11-28", 5), "2026-12-03");
        assert_eq!(day_number("2026-11-02") - day_number("2026-10-03"), 30);
        let tokyo = parse_instant("2026-11-05T10:00+09:00", -300).unwrap();
        assert_eq!(format_instant(tokyo, -300), "2026-11-04T20:00:00-05:00");
        assert_eq!(parse_instant("2026-11-04T20:00", -300), Some(tokyo));
        assert_eq!(parse_instant("2026-11-05T01:00:00Z", -300), Some(tokyo));
        assert!(parse_instant("next tuesday", -300).is_none());
    }

    #[test]
    fn transfer_checks_payee_and_balance() {
        let world = World::new(json!({
            "accounts": [{"id": "chk", "type": "checking", "balance": 100.0}],
            "payees": [{"id": "p1", "name": "Landlord"}],
        }));
        let unknown = world.call(
            "bank",
            json!({"action": "transfer", "from_account": "chk", "payee": "Bob", "amount": 5}),
        );
        assert!(unknown.is_err());
        let too_much = world.call("bank", json!({"action": "transfer", "from_account": "chk", "payee": "landlord", "amount": 500}));
        assert!(too_much.is_err());
        world
            .call("bank", json!({"action": "transfer", "from_account": "chk", "payee": "Landlord", "amount": 40}))
            .unwrap();
        assert_eq!(world.state()["accounts"][0]["balance"], json!(60.0));
        assert_eq!(world.log().len(), 3);
    }

    #[test]
    fn outbound_mail_triggers_a_scripted_reply_once() {
        let world = World::new(json!({
            "user": {"email": "me@example.com"},
            "auto_replies": [{"channel": "email", "match": {"to": "dana@", "any": ["dinner"]},
                               "reply": {"from": "dana@example.com", "body": "Friday works"}}],
        }));
        let send = json!({"action": "send", "to": ["dana@example.com"], "subject": "Dinner?", "body": "Free for dinner?"});
        let first = world.call("email", send.clone()).unwrap();
        assert_eq!(first["new_messages"][0]["subject"], "Re: Dinner?");
        let second = world.call("email", send).unwrap();
        assert!(second.get("new_messages").is_none());
        let inbox: Vec<Json> = world.state()["emails"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["folder"] == "inbox")
            .cloned()
            .collect();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0]["subject"], "Re: Dinner?");
    }
}
