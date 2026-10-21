//! The local and remote terminals share native dialog controls and validation.
use std::any::Any;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_plugin_sdk::dialogs::native::{Dialog, DialogKind};
use rpi_tui::{
    Component, Container, Focusable, Input, SearchableSelectList, SelectItem, SelectList,
    SettingItem, SettingsList, Spacer, Text,
};
use serde_json::{json, Value};

mod search;
use search::filter as fuzzy_filter;

type Reply = Arc<dyn Fn(Option<Value>) + Send + Sync>;
enum Control {
    Input(Arc<Input>),
    Select(Arc<SelectList>, Option<Arc<SearchableSelectList>>),
    Settings(Arc<SettingsList>, Option<Arc<Input>>),
}

pub(crate) struct NativeDialogView {
    frame: Container,
    control: Control,
    error: Arc<Text>,
    focused: AtomicBool,
}

fn query(input: &Option<Weak<Input>>) -> String {
    input
        .as_ref()
        .and_then(Weak::upgrade)
        .map(|input| input.get_value())
        .unwrap_or_default()
}

impl NativeDialogView {
    pub(crate) fn new(dialog: Dialog, reply: Reply) -> Self {
        let dialog = Arc::new(dialog);
        let error = Arc::new(Text::new("", 0, 0));
        let done = AtomicBool::new(false);
        let finish: Reply = Arc::new({
            let dialog = dialog.clone();
            let error = error.clone();
            move |answer| {
                let answer = match answer.map(|answer| dialog.answer(&answer)).transpose() {
                    Ok(answer) => answer,
                    Err(message) => {
                        error.set_text(message);
                        return;
                    }
                };
                if !done.swap(true, Ordering::AcqRel) {
                    reply(answer);
                }
            }
        });
        let frame = Container::new();
        frame.add_child(Arc::new(Text::new(&dialog.title, 0, 0)));
        if let Some(description) = &dialog.description {
            frame.add_child(Arc::new(Text::new(description, 0, 0)));
        }
        frame.add_child(Arc::new(Spacer::new(1)));
        let control = match dialog.kind {
            DialogKind::Input => {
                let input = Arc::new(Input::with_placeholder(
                    dialog.placeholder.as_deref().unwrap_or("Type your answer…"),
                ));
                input.set_value(&dialog.initial_value);
                input.set_masked(dialog.masked);
                let submit = finish.clone();
                input.on_submit(Arc::new(move |value| {
                    submit(Some(json!({"type":"input","value":value})))
                }));
                input.on_escape(Arc::new(move || finish(None)));
                frame.add_child(input.clone());
                Control::Input(input)
            }
            DialogKind::Select | DialogKind::Confirm => {
                let mut items: Vec<SelectItem> = if dialog.kind == DialogKind::Confirm {
                    vec![
                        SelectItem::new("true", "Yes"),
                        SelectItem::new("false", "No"),
                    ]
                } else {
                    dialog
                        .choices
                        .iter()
                        .enumerate()
                        .map(|(index, choice)| {
                            let mut item = SelectItem::new(&index.to_string(), &choice.label)
                                .with_search_text(&choice.label);
                            if let Some(description) = &choice.description {
                                item = item.with_description(description);
                            }
                            item
                        })
                        .collect()
                };
                if dialog.freeform {
                    items.push(
                        SelectItem::new("freeform", "Type another answer…")
                            .with_search_text("Type another answer…"),
                    );
                }
                let list = Arc::new(if dialog.multiple {
                    SelectList::new_multi(items.clone(), 10)
                } else {
                    SelectList::new(items.clone(), 10)
                });
                let search = dialog.searchable.then(|| {
                    Arc::new(SearchableSelectList::with_filter(
                        None,
                        None,
                        list.clone(),
                        |list, query| {
                            list.set_filter_with(query, |items, query| {
                                fuzzy_filter(items, query, SelectItem::search_key)
                            });
                        },
                    ))
                });
                if let Some(search) = &search {
                    search.set_search_text(&dialog.query);
                }
                if let Some(selected) = &dialog.selected {
                    let target = if dialog.kind == DialogKind::Confirm {
                        Some(selected.clone())
                    } else {
                        dialog
                            .choices
                            .iter()
                            .position(|choice| &choice.value == selected)
                            .map(|index| index.to_string())
                    };
                    let filtered = fuzzy_filter(
                        &items,
                        if dialog.searchable { &dialog.query } else { "" },
                        SelectItem::search_key,
                    );
                    if let Some(index) = filtered
                        .iter()
                        .position(|item| Some(&item.value) == target.as_ref())
                    {
                        list.set_selected_index(index);
                    }
                }
                let input = search.as_ref().map(|search| Arc::downgrade(search.input()));
                let select = Arc::new({
                    let dialog = dialog.clone();
                    let finish = finish.clone();
                    move |items: &[SelectItem]| {
                        if dialog.kind == DialogKind::Confirm {
                            if let Some(item) = items.first() {
                                finish(Some(
                                    json!({"type":"confirm","confirmed":item.value == "true"}),
                                ));
                            }
                            return;
                        }
                        let freeform = items.iter().any(|item| item.value == "freeform");
                        let values: Vec<&str> = items
                            .iter()
                            .filter_map(|item| item.value.parse::<usize>().ok())
                            .filter_map(|index| dialog.choices.get(index))
                            .map(|choice| choice.value.as_str())
                            .collect();
                        finish(Some(
                            json!({"type":if freeform {"freeform"} else {"select"},"values":values,"query":query(&input)}),
                        ));
                    }
                });
                let single = select.clone();
                list.on_select(Arc::new(move |item| single(std::slice::from_ref(item))));
                list.on_multi_select(select);
                list.on_cancel(Arc::new(move || finish(None)));
                if let Some(search) = &search {
                    frame.add_child(search.clone());
                } else {
                    frame.add_child(list.clone());
                }
                Control::Select(list, search)
            }
            DialogKind::Settings => {
                let items: Vec<SettingItem> = dialog
                    .settings
                    .iter()
                    .map(|setting| {
                        let mut item =
                            SettingItem::new(&setting.id, &setting.label, &setting.value)
                                .with_values(
                                    &setting
                                        .values
                                        .iter()
                                        .map(String::as_str)
                                        .collect::<Vec<_>>(),
                                );
                        if let Some(description) = &setting.description {
                            item = item.with_description(description);
                        }
                        if setting.submenu {
                            item = item.with_submenu();
                        }
                        item
                    })
                    .collect();
                let filtered = fuzzy_filter(
                    &items,
                    if dialog.searchable { &dialog.query } else { "" },
                    |item| &item.label,
                );
                let list = Arc::new(SettingsList::new(filtered.clone()));
                if let Some(index) = filtered
                    .iter()
                    .position(|item| Some(&item.key) == dialog.selected.as_ref())
                {
                    list.set_selected(index);
                }
                let search = dialog.searchable.then(|| {
                    let input = Arc::new(Input::with_placeholder("Type to filter…"));
                    input.set_value(&dialog.query);
                    let list = list.clone();
                    input.on_change(Arc::new(move |value| {
                        list.set_items(fuzzy_filter(&items, value, |item| &item.label))
                    }));
                    frame.add_child(input.clone());
                    input
                });
                let input = search.as_ref().map(Arc::downgrade);
                let changed = finish.clone();
                let changed_query = input.clone();
                list.on_change(Arc::new(move |id, value| changed(Some(json!({"type":"change","id":id,"value":value,"query":query(&changed_query)})))));
                let opened = finish.clone();
                list.on_select(Arc::new(move |item| {
                    opened(Some(
                        json!({"type":"open","id":item.key,"query":query(&input)}),
                    ))
                }));
                list.on_cancel(Arc::new(move || finish(None)));
                frame.add_child(list.clone());
                Control::Settings(list, search)
            }
        };
        frame.add_child(error.clone());
        let hint = match dialog.kind {
            DialogKind::Input => "Enter submit · Esc cancel",
            DialogKind::Settings => "Enter/Space change · Esc cancel",
            _ if dialog.multiple => "Space toggle · Enter submit · Esc cancel",
            _ => "Enter select · Esc cancel",
        };
        frame.add_child(Arc::new(Text::new(hint, 0, 0)));
        Self {
            frame,
            control,
            error,
            focused: AtomicBool::new(false),
        }
    }

    pub(crate) fn handle_key(&self, key: KeyEvent) {
        self.error.set_text("");
        match &self.control {
            Control::Input(input) => input.handle_key(key),
            Control::Select(_, Some(search)) => search.handle_key(key),
            Control::Select(list, None) => list.handle_key(key),
            Control::Settings(list, search) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('k' | 'p') if ctrl => list.move_up(),
                    KeyCode::Char('j' | 'n') if ctrl => list.move_down(),
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Up | KeyCode::Down => {
                        list.handle_key(key)
                    }
                    KeyCode::Char(' ')
                        if search
                            .as_ref()
                            .map_or(true, |input| input.get_value().is_empty()) =>
                    {
                        list.handle_key(key)
                    }
                    _ => {
                        if let Some(input) = search {
                            input.handle_key(key);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn handle_paste(&self, text: &str) {
        self.error.set_text("");
        if let Some(input) = self.input() {
            input.handle_paste(text);
        }
    }

    fn input(&self) -> Option<&Arc<Input>> {
        match &self.control {
            Control::Input(input) => Some(input),
            Control::Select(_, Some(search)) => Some(search.input()),
            Control::Settings(_, Some(input)) => Some(input),
            _ => None,
        }
    }
}

impl Component for NativeDialogView {
    fn render(&self, width: usize) -> Vec<String> {
        self.frame.render(width)
    }
    fn invalidate(&self) {
        self.frame.invalidate();
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl Focusable for NativeDialogView {
    fn set_focused(&self, focused: bool) {
        self.focused.store(focused, Ordering::Release);
        if let Some(input) = self.input() {
            input.set_focused(focused);
        }
    }
    fn is_focused(&self) -> bool {
        self.focused.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};

    fn view(spec: Value) -> (NativeDialogView, Receiver<Option<Value>>) {
        let (tx, rx) = channel();
        let view = NativeDialogView::new(
            Dialog::parse(&spec).unwrap(),
            Arc::new(move |answer| {
                tx.send(answer).unwrap();
            }),
        );
        (view, rx)
    }
    fn key(view: &NativeDialogView, code: KeyCode) {
        view.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn search_and_reopened_selection_return_stable_ids_once() {
        let (view, rx) = view(
            json!({"kind":"select","title":"Accounts","searchable":true,"query":"same","selected":"two",
            "choices":[{"value":"other","label":"Different"},{"value":"one","label":"Same"},{"value":"two","label":"Same"}]}),
        );
        key(&view, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"select","values":["two"],"query":"same"}))
        );
        key(&view, KeyCode::Enter);
        key(&view, KeyCode::Esc);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn freeform_preserves_multiple_selected_ids() {
        let (view, rx) = view(
            json!({"kind":"select","title":"Choices","multiple":true,"freeform":true,
            "choices":[{"value":"one","label":"First"},{"value":"two","label":"Second"}]}),
        );
        key(&view, KeyCode::Char(' '));
        key(&view, KeyCode::Down);
        key(&view, KeyCode::Down);
        key(&view, KeyCode::Char(' '));
        key(&view, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"freeform","values":["one"],"query":""}))
        );
    }

    #[test]
    fn search_retains_checked_items_when_they_are_filtered_out() {
        let (view, rx) = view(
            json!({"kind":"select","title":"Choices","multiple":true,"searchable":true,
            "choices":[{"value":"one","label":"First"},{"value":"two","label":"Second"}]}),
        );
        key(&view, KeyCode::Char(' '));
        view.handle_paste("Second");
        key(&view, KeyCode::Char(' '));
        key(&view, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"select","values":["one","two"],"query":"Second"}))
        );
    }

    #[test]
    fn masked_inputs_validate_without_leaking_the_supplied_text() {
        let (view, rx) = view(
            json!({"kind":"input","title":"Weight","masked":true,"integerMin":1,"initialValue":"private-bad-number"}),
        );
        key(&view, KeyCode::Enter);
        assert!(rx.try_recv().is_err());
        let rendered = view.render(80).join("\n");
        assert!(!rendered.contains("private-bad-number"));
        assert!(rendered.contains("Enter an integer greater than or equal to 1."));
        view.input().unwrap().set_value("");
        view.handle_paste(" 002 ");
        key(&view, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"input","value":"2"}))
        );
    }

    #[test]
    fn settings_searches_labels_restores_submenus_and_retains_query() {
        let settings = json!([{"id":"unrelated-key","label":"Pool enabled","value":"yes","values":["yes","no"]},
            {"id":"accounts-id","label":"Pool accounts","value":"2","submenu":true}]);
        let (first, rx) = view(
            json!({"kind":"settings","title":"Pool","searchable":true,"query":"pool","selected":"accounts-id","settings":settings}),
        );
        key(&first, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"open","id":"accounts-id","query":"pool"}))
        );
        let (second, rx) =
            view(json!({"kind":"settings","title":"Pool","searchable":true,"settings":settings}));
        second.handle_paste("enabled");
        key(&second, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"change","id":"unrelated-key","value":"no","query":"enabled"}))
        );
        let (third, rx) =
            view(json!({"kind":"settings","title":"Pool","searchable":true,"settings":settings}));
        third.handle_paste("Pool");
        key(&third, KeyCode::Char(' '));
        assert!(rx.try_recv().is_err());
        third.handle_paste("accounts");
        key(&third, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"open","id":"accounts-id","query":"Pool accounts"}))
        );
    }

    #[test]
    fn cancellation_confirmation_and_component_release() {
        let (cancelled, rx) = view(json!({"kind":"input","title":"Login"}));
        key(&cancelled, KeyCode::Esc);
        assert_eq!(rx.try_recv().unwrap(), None);
        let (confirmed, rx) = view(json!({"kind":"confirm","title":"Remove?","selected":"false"}));
        key(&confirmed, KeyCode::Enter);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(json!({"type":"confirm","confirmed":false}))
        );
        for kind in ["settings", "select"] {
            let (view, _) = view(json!({"kind":kind,"title":"Search","searchable":true}));
            let input = Arc::downgrade(view.input().unwrap());
            drop(view);
            assert!(
                input.upgrade().is_none(),
                "search callbacks must not form an ownership cycle"
            );
        }
    }
}
