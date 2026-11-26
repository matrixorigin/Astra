#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use tokio::sync::oneshot;

use super::{BottomPane, BottomPaneAction};
use crate::cli::chat_stream::{
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

fn single_question(freeform: bool) -> AskUserPrompt {
    AskUserPrompt {
        context: None,
        questions: vec![AskUserQuestion {
            id: None,
            header: "Question".into(),
            question: "Answer?".into(),
            options: if freeform {
                vec![]
            } else {
                vec![choice("One", None)]
            },
            multi_select: false,
            allow_freeform: freeform,
        }],
        timeout_ms: None,
    }
}

#[test]
fn ask_user_closed_request_does_not_keep_a_modal_open() {
    let mut bp = BottomPane::new();
    let prompt = AskUserPrompt {
        context: None,
        questions: vec![AskUserQuestion {
            id: None,
            header: "Direction".into(),
            question: "Choose a direction".into(),
            options: vec![],
            multi_select: false,
            allow_freeform: true,
        }],
        timeout_ms: None,
    };
    let (tx, rx) = oneshot::channel();
    drop(rx);
    bp.enqueue_ask_user(prompt.clone(), tx);
    assert!(!bp.has_active_view());

    let rx = enqueue(&mut bp, prompt);
    assert!(bp.has_active_view());
    drop(rx);
    assert!(bp.pre_draw_tick(std::time::Instant::now()));
    assert!(!bp.has_active_view());
}

#[test]
fn native_questionnaire_preserves_ids_with_identical_labels() {
    let mut bp = BottomPane::new();
    let prompt = astra_tools::ask_user::parse_ask_user_prompt(&serde_json::json!({
        "questions": [
            {"id":"first", "header":"Choice", "question":"Proceed?", "options":[{"label":"Yes"},{"label":"No"}]},
            {"id":"second", "header":"Choice", "question":"Proceed?", "options":[{"label":"Yes"},{"label":"No"}]}
        ]
    })).unwrap();
    let mut rx = enqueue(&mut bp, prompt.clone());
    let _ = bp.handle_key(key('1'));
    let _ = bp.handle_key(key('2'));
    let _ = bp.handle_key(special(KeyCode::Tab));
    let _ = bp.handle_key(special(KeyCode::Enter));
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("questionnaire was not submitted");
    };
    let answers = astra_tools::ask_user::normalize_ask_user_answers(&prompt, &answers).unwrap();
    let result = answers.to_tool_result_value();
    assert_eq!(result["answers"]["first"], serde_json::json!("Yes"));
    assert_eq!(result["answers"]["second"], serde_json::json!("No"));
}

#[test]
fn ask_user_single_question_digit_submits_answer() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: Some("The agent needs a product decision.".into()),
            questions: vec![AskUserQuestion {
                id: None,
                header: "Stack".into(),
                question: "Which implementation should we use?".into(),
                options: vec![
                    choice("Native TUI", Some("Best UX")),
                    choice("Plain text", None),
                ],
                multi_select: false,
                allow_freeform: true,
            }],
            timeout_ms: None,
        },
    );

    let action = bp.handle_key(key('1'));
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                id: None,
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
                    id: None,
                    header: "Frontend".into(),
                    question: "Which frontend stack should we use?".into(),
                    options: vec![choice("React + TS", None), choice("Vue 3 + TS", None)],
                    multi_select: false,
                    allow_freeform: true,
                },
                AskUserQuestion {
                    id: None,
                    header: "Features".into(),
                    question: "Which features should we include first?".into(),
                    options: vec![choice("RBAC", None), choice("Reports", None)],
                    multi_select: true,
                    allow_freeform: true,
                },
            ],
            timeout_ms: None,
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
                    id: None,
                    header: "Frontend".into(),
                    question: "Which frontend stack should we use?".into(),
                    options: vec![choice("React + TS", None), choice("Vue 3 + TS", None)],
                    multi_select: false,
                    allow_freeform: false,
                },
                AskUserQuestion {
                    id: None,
                    header: "Features".into(),
                    question: "Which features should we include first?".into(),
                    options: vec![choice("RBAC", None), choice("Reports", None)],
                    multi_select: true,
                    allow_freeform: false,
                },
            ],
            timeout_ms: None,
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
                    id: None,
                    question: "Which frontend stack should we use?".into(),
                    answers: vec!["React + TS".into()],
                    multi_select: false,
                    annotation: None,
                },
                AskUserQuestionAnswer {
                    id: None,
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
                id: None,
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
            timeout_ms: None,
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
                id: None,
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
            timeout_ms: None,
        },
    );

    let _ = bp.handle_key(key('1'));
    let _ = bp.handle_key(key('n'));
    bp.handle_paste("ship it");
    let _ = bp.handle_key(special(KeyCode::Enter));
    let _ = bp.handle_key(special(KeyCode::Tab));
    let action = bp.handle_key(special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                id: None,
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
                id: None,
                header: "限制".into(),
                question: "请说明你的偏好技术栈，以及还有什么额外限制。".into(),
                options: vec![choice("前后端分离", None), choice("单体应用", None)],
                multi_select: false,
                allow_freeform: true,
            }],
            timeout_ms: None,
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
                id: None,
                header: "Name".into(),
                question: "What should we name this command?".into(),
                options: vec![],
                multi_select: false,
                allow_freeform: true,
            }],
            timeout_ms: None,
        },
    );

    bp.handle_paste("astra ask");
    assert!(
        bp.composer.is_empty(),
        "paste must stay in the focused question"
    );
    let action = bp.handle_key(special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(
        rx.try_recv().unwrap(),
        AskUserResponse::Submitted(AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                id: None,
                question: "What should we name this command?".into(),
                answers: vec!["astra ask".into()],
                multi_select: false,
                annotation: None,
            }],
        })
    );
}

#[test]
fn paste_without_a_question_text_field_does_not_reach_the_draft() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let _rx = enqueue(&mut bp, single_question(false));
    bp.handle_paste("must not enter the conversation");
    assert!(bp.composer.is_empty());
    assert!(render_text(&bp, 80, 24).contains("Paste not inserted"));
}

#[test]
fn unbracketed_paste_cannot_submit_a_partial_answer() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, single_question(true));
    let now = std::time::Instant::now();
    for (index, code) in [
        KeyCode::Char('a'),
        KeyCode::Char('b'),
        KeyCode::Char('c'),
        KeyCode::Tab,
        KeyCode::Enter,
    ]
    .into_iter()
    .enumerate()
    {
        let action = bp.handle_key_at(
            special(code),
            now + std::time::Duration::from_millis(index as u64 + 1),
        );
        assert!(!matches!(action, BottomPaneAction::ViewCompleted { .. }));
    }
    assert!(rx.try_recv().is_err(), "pasted newline must not submit");
    let action = bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(200),
    );
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["abc"]);
}

#[test]
fn paste_keeps_its_owner_when_a_question_opens() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let now = std::time::Instant::now();
    bp.handle_key_at(key('a'), now);
    let mut rx = enqueue(&mut bp, single_question(false));
    for (i, code) in [KeyCode::Char('b'), KeyCode::Char('c'), KeyCode::Enter]
        .into_iter()
        .enumerate()
    {
        bp.handle_key_at(
            special(code),
            now + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    assert!(
        rx.try_recv().is_err(),
        "paste must not choose a default answer"
    );
    assert!(bp.has_active_view());
    assert_eq!(bp.composer.text(), "abc\n");
}

#[test]
fn canceling_a_question_discards_its_paste_tail_without_touching_the_draft() {
    for cancel in [
        special(KeyCode::Esc),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut bp = BottomPane::new();
        bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
        bp.composer.set_text("parked draft");
        let mut rx = enqueue(&mut bp, single_question(true));
        let now = std::time::Instant::now();
        for (i, ch) in "abc".chars().enumerate() {
            bp.handle_key_at(
                key(ch),
                now + std::time::Duration::from_millis(i as u64 + 1),
            );
        }
        bp.handle_key_at(cancel, now + std::time::Duration::from_millis(4));
        assert!(matches!(rx.try_recv(), Ok(AskUserResponse::Cancelled)));
        bp.handle_key_at(key('x'), now + std::time::Duration::from_millis(5));
        let action = bp.handle_key_at(
            special(KeyCode::Enter),
            now + std::time::Duration::from_millis(6),
        );
        assert!(matches!(action, BottomPaneAction::Consumed));
        assert_eq!(bp.composer.text(), "parked draft");
    }
}

#[test]
fn fresh_typing_in_a_new_question_does_not_go_to_the_parked_draft() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let now = std::time::Instant::now();
    for (i, ch) in "abc".chars().enumerate() {
        bp.handle_key_at(key(ch), now + std::time::Duration::from_millis(i as u64));
    }
    let mut rx = enqueue(&mut bp, single_question(true));
    bp.pre_draw_tick(now + std::time::Duration::from_millis(40));
    bp.handle_key_at(key('x'), now + std::time::Duration::from_millis(41));
    assert_eq!(bp.composer.text(), "abc");
    let action = bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(75),
    );
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["x"]);
}

#[test]
fn fresh_question_input_releases_an_unflushed_composer_paste() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let now = std::time::Instant::now();
    for (i, ch) in "abc".chars().enumerate() {
        bp.handle_key_at(key(ch), now + std::time::Duration::from_millis(i as u64));
    }
    let mut rx = enqueue(&mut bp, single_question(true));
    bp.handle_key_at(key('x'), now + std::time::Duration::from_millis(20));
    assert_eq!(bp.composer.text(), "abc");
    let action = bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(55),
    );
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["x"]);
}

#[test]
fn bracketed_question_paste_releases_the_parked_composers_canceled_tail() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    bp.composer.set_text("parked draft");
    let now = std::time::Instant::now();
    bp.handle_key_at(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        now,
    );
    for (i, ch) in "abc".chars().enumerate() {
        bp.handle_key_at(
            key(ch),
            now + std::time::Duration::from_millis(i as u64 + 1),
        );
    }
    bp.handle_key_at(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        now + std::time::Duration::from_millis(4),
    );
    let mut rx = enqueue(&mut bp, single_question(true));
    bp.handle_paste("P");
    bp.handle_key_at(key('x'), now + std::time::Duration::from_millis(5));
    bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(25),
    );
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["Px"]);
    assert_eq!(bp.composer.text(), "parked draft");
}

#[test]
fn bracketed_question_paste_can_be_submitted_immediately() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    let mut rx = enqueue(&mut bp, single_question(true));
    let now = std::time::Instant::now();
    bp.handle_key_at(key('x'), now);
    bp.handle_paste("P");
    let action = bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(1),
    );
    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    let AskUserResponse::Submitted(answers) = rx.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["xP"]);
}

#[test]
fn canceling_a_question_discards_its_tail_even_if_another_question_opens() {
    let mut bp = BottomPane::new();
    bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
    bp.composer.set_text("parked draft");
    let mut first = enqueue(&mut bp, single_question(true));
    let now = std::time::Instant::now();
    for (i, ch) in "abc".chars().enumerate() {
        bp.handle_key_at(key(ch), now + std::time::Duration::from_millis(i as u64));
    }
    bp.handle_key_at(
        special(KeyCode::Esc),
        now + std::time::Duration::from_millis(3),
    );
    assert!(matches!(first.try_recv(), Ok(AskUserResponse::Cancelled)));
    let mut second = enqueue(&mut bp, single_question(true));
    for (i, code) in [KeyCode::Char('x'), KeyCode::Enter].into_iter().enumerate() {
        bp.handle_key_at(
            special(code),
            now + std::time::Duration::from_millis(4 + i as u64),
        );
    }
    assert!(second.try_recv().is_err());
    assert_eq!(bp.composer.text(), "parked draft");
    bp.handle_key_at(key('y'), now + std::time::Duration::from_millis(200));
    bp.handle_key_at(
        special(KeyCode::Enter),
        now + std::time::Duration::from_millis(400),
    );
    let AskUserResponse::Submitted(answers) = second.try_recv().unwrap() else {
        panic!("expected an answer");
    };
    assert_eq!(answers.answers[0].answers, ["y"]);
}

#[test]
fn closing_a_question_does_not_restart_the_paste_guard() {
    for (cancel_after, next_after, code) in
        [(40, 41, KeyCode::Char('x')), (125, 131, KeyCode::Enter)]
    {
        let mut bp = BottomPane::new();
        bp.composer = super::chat_composer::ChatComposer::new_ephemeral();
        bp.composer.set_text("parked draft");
        let _rx = enqueue(&mut bp, single_question(true));
        let now = std::time::Instant::now();
        for (i, ch) in "abc".chars().enumerate() {
            bp.handle_key_at(key(ch), now + std::time::Duration::from_millis(i as u64));
        }
        bp.pre_draw_tick(now + std::time::Duration::from_millis(10));
        bp.handle_key_at(
            special(KeyCode::Esc),
            now + std::time::Duration::from_millis(cancel_after),
        );
        let action = bp.handle_key_at(
            special(code),
            now + std::time::Duration::from_millis(next_after),
        );
        match code {
            KeyCode::Char(_) => assert_eq!(bp.composer.text(), "parked draftx"),
            KeyCode::Enter => assert!(matches!(
                action,
                BottomPaneAction::SubmitInput(ref text) if text == "parked draft"
            )),
            _ => unreachable!(),
        }
    }
}

#[test]
fn ask_user_escape_reports_cancellation_not_answer_sentinel() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        AskUserPrompt {
            context: None,
            questions: vec![AskUserQuestion {
                id: None,
                header: "Continue".into(),
                question: "Continue?".into(),
                options: vec![choice("Yes", None), choice("No", None)],
                multi_select: false,
                allow_freeform: true,
            }],
            timeout_ms: None,
        },
    );

    let action = bp.handle_key(special(KeyCode::Esc));

    assert!(matches!(action, BottomPaneAction::ViewCompleted { .. }));
    assert_eq!(rx.try_recv().unwrap(), AskUserResponse::Cancelled);
}
