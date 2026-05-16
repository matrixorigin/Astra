#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use tokio::sync::oneshot;

use super::{BottomPane, BottomPaneAction};
use crate::chat_stream::{
    AskUserAnnotation, AskUserAnswers, AskUserChoice, AskUserPrompt, AskUserQuestion,
    AskUserQuestionAnswer, AskUserResponse,
};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn enqueue(bp: &mut BottomPane, prompt: AskUserPrompt) -> oneshot::Receiver<AskUserResponse> {
    let (tx, rx) = oneshot::channel();
    bp.enqueue_ask_user(prompt, tx);
    rx
}

fn render_text(bp: &BottomPane, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    bp.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn choice(label: &str, description: Option<&str>) -> AskUserChoice {
    AskUserChoice {
        label: label.into(),
        description: description.map(ToString::to_string),
        preview: None,
    }
}

#[test]
fn ask_user_single_question_digit_submits_answer() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: Some("The agent needs a product decision.".into()),
            questions: vec![AskUserQuestion {
                header: "Stack".into(),
                question: "Which implementation should we use?".into(),
                options: vec![
                    choice("Native TUI", Some("Best UX")),
                    choice("Plain text", None),
                ],
                multi_select: false,
                allow_freeform: true,
            }],
        },
    );

    let action = bp.handle_key(key('1'));
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                question: "Which implementation should we use?".into(),
                answers: vec!["Native TUI".into()],
                multi_select: false,
                annotation: None,
            }],
        })
    );
}

#[test]
fn ask_user_questionnaire_renders_tabs_and_multi_select_checkboxes() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: Some("Need to pin the first version.".into()),
            questions: vec![
                AskUserQuestion {
                    header: "Frontend".into(),
                    question: "Which frontend stack should we use?".into(),
                    options: vec![choice("React + TS", None), choice("Vue 3 + TS", None)],
                    multi_select: false,
                    allow_freeform: true,
                },
                AskUserQuestion {
                    header: "Features".into(),
                    question: "Which features should we include first?".into(),
                    options: vec![choice("RBAC", None), choice("Reports", None)],
                    multi_select: true,
                    allow_freeform: true,
                },
            ],
        },
    );

    let _ = bp.handle_key(special(KeyCode::Tab));
    let text = render_text(&bp, 80, 18);
    assert!(text.contains("Frontend"), "missing tab chip: {text}");
    assert!(text.contains("Features"), "missing second tab chip: {text}");
    assert!(text.contains("Submit"), "missing submit tab: {text}");
    assert!(
        text.contains("[ ]"),
        "multi-select should render checkboxes: {text}"
    );
    assert!(
        text.contains("Other"),
        "freeform path should be visible: {text}"
    );
}

#[test]
fn ask_user_questionnaire_submits_answers_across_tabs() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![
                AskUserQuestion {
                    header: "Frontend".into(),
                    question: "Which frontend stack should we use?".into(),
                    options: vec![choice("React + TS", None), choice("Vue 3 + TS", None)],
                    multi_select: false,
                    allow_freeform: false,
                },
                AskUserQuestion {
                    header: "Features".into(),
                    question: "Which features should we include first?".into(),
                    options: vec![choice("RBAC", None), choice("Reports", None)],
                    multi_select: true,
                    allow_freeform: false,
                },
            ],
        },
    );

    let _ = bp.handle_key(key('1'));
    let _ = bp.handle_key(key('1'));
    let _ = bp.handle_key(special(KeyCode::Tab));
    let action = bp.handle_key(special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![
                AskUserQuestionAnswer {
                    question: "Which frontend stack should we use?".into(),
                    answers: vec!["React + TS".into()],
                    multi_select: false,
                    annotation: None,
                },
                AskUserQuestionAnswer {
                    question: "Which features should we include first?".into(),
                    answers: vec!["RBAC".into()],
                    multi_select: true,
                    annotation: None,
                },
            ],
        })
    );
}

#[test]
fn preview_question_renders_preview_panel_and_notes_hint() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![AskUserQuestion {
                header: "Layout".into(),
                question: "Which layout should we use?".into(),
                options: vec![
                    AskUserChoice {
                        label: "Cards".into(),
                        description: Some("Two-column layout".into()),
                        preview: Some("card-a\ncard-b".into()),
                    },
                    AskUserChoice {
                        label: "Table".into(),
                        description: Some("Dense list".into()),
                        preview: Some("row-a\nrow-b".into()),
                    },
                ],
                multi_select: false,
                allow_freeform: false,
            }],
        },
    );

    let text = render_text(&bp, 100, 20);
    assert!(text.contains("card-a"), "missing preview panel: {text}");
    assert!(text.contains("Notes"), "missing notes area: {text}");
}

#[test]
fn preview_question_submits_notes_and_selected_preview_annotation() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![AskUserQuestion {
                header: "Layout".into(),
                question: "Which layout should we use?".into(),
                options: vec![
                    AskUserChoice {
                        label: "Cards".into(),
                        description: None,
                        preview: Some("card-a\ncard-b".into()),
                    },
                    AskUserChoice {
                        label: "Table".into(),
                        description: None,
                        preview: Some("row-a\nrow-b".into()),
                    },
                ],
                multi_select: false,
                allow_freeform: false,
            }],
        },
    );

    let _ = bp.handle_key(key('1'));
    let _ = bp.handle_key(key('n'));
    for ch in "ship it".chars() {
        let _ = bp.handle_key(key(ch));
    }
    let _ = bp.handle_key(special(KeyCode::Enter));
    let _ = bp.handle_key(special(KeyCode::Tab));
    let action = bp.handle_key(special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                question: "Which layout should we use?".into(),
                answers: vec!["Cards".into()],
                multi_select: false,
                annotation: Some(AskUserAnnotation {
                    notes: Some("ship it".into()),
                    preview: Some("card-a\ncard-b".into()),
                }),
            }],
        })
    );
}

#[test]
fn freeform_other_renders_visible_input_box_and_cursor() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: Some("需要先补齐需求信息。".into()),
            questions: vec![AskUserQuestion {
                header: "限制".into(),
                question: "请说明你的偏好技术栈，以及还有什么额外限制。".into(),
                options: vec![choice("前后端分离", None), choice("单体应用", None)],
                multi_select: false,
                allow_freeform: true,
            }],
        },
    );

    let _ = bp.handle_key(special(KeyCode::Down));
    let _ = bp.handle_key(special(KeyCode::Down));
    let text = render_text(&bp, 80, 18);
    assert!(text.contains("Your answer"), "missing answer box: {text}");
    assert!(
        text.contains("Type your answer"),
        "missing freeform placeholder: {text}"
    );
    assert!(
        bp.cursor_position(Rect::new(0, 0, 80, 18)).is_some(),
        "freeform prompt should expose a visible cursor"
    );
}

#[test]
fn freeform_only_question_without_options_submits_typed_answer() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![AskUserQuestion {
                header: "Name".into(),
                question: "What should we name this command?".into(),
                options: vec![],
                multi_select: false,
                allow_freeform: true,
            }],
        },
    );

    for ch in "astra ask".chars() {
        let _ = bp.handle_key(key(ch));
    }
    let action = bp.handle_key(special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                question: "What should we name this command?".into(),
                answers: vec!["astra ask".into()],
                multi_select: false,
                annotation: None,
            }],
        })
    );
}

#[test]
fn ask_user_escape_reports_cancellation_not_answer_sentinel() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![AskUserQuestion {
                header: "Continue".into(),
                question: "Continue?".into(),
                options: vec![choice("Yes", None), choice("No", None)],
                multi_select: false,
                allow_freeform: true,
            }],
        },
    );

    let action = bp.handle_key(special(KeyCode::Esc));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(rx.try_recv().unwrap(), AskUserResponse::Cancelled);
}
