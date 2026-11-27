//! A batch line whose body would parse into more structure than a request
//! body may (`inference::json_weight`, #872) is refused at planning by its
//! line number, as any invalid line, with nothing held.
use super::*;

#[dbtest::test]
async fn a_line_that_parses_too_large_is_refused_at_planning(pool: PgPool) {
    let w = world(pool, false, false, 1).await;
    let f = &w.f;
    let mut hostile = chat_line("b", "company/claude", "hi");
    hostile["body"]["tool_choice"] = json!(vec![json!({"a": 0}); 200_000]);
    let file = w
        .upload(
            &f.principal,
            &[chat_line("a", "company/claude", "hi"), hostile],
        )
        .await;
    let before = admission_rowcounts(f).await;
    let Err(CreateError::Invalid(issues)) = w
        .create(f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
    else {
        panic!("the line is refused at planning");
    };
    assert_eq!(
        issues
            .iter()
            .map(|i| (i.line, i.error.code()))
            .collect::<Vec<_>>(),
        [(2, "line_too_large")]
    );
    assert_eq!(admission_rowcounts(f).await, before, "nothing held");
    assert!(w.anthropic.executed().is_empty());
    consistent(f).await;
}
