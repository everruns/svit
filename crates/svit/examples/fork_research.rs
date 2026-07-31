use svit::{Process, Value, value};

const SCORE_EVIDENCE: &str = r#"
function main(input)
    local score = 0
    for _, item in ipairs(memory.evidence) do
        score = score
            + item.impact * input.impact_weight
            - item.cost * input.cost_weight
    end
    memory.analysis = { lens = input.lens, score = score }
    return memory.analysis
end
"#;

fn main() -> svit::Result<()> {
    let mut parent = Process::builder("svit://local/examples/research")?
        .memory(value!({
            "evidence": [
                {"claim": "faster iteration", "impact": 8, "cost": 7},
                {"claim": "better isolation", "impact": 9, "cost": 4}
            ]
        }))
        .build()?;
    parent.save_script("score_evidence", SCORE_EVIDENCE)?;

    let mut growth = parent.fork("svit://local/examples/research-growth")?;
    let mut efficiency = parent.fork("svit://local/examples/research-efficiency")?;

    let growth_result = growth.run(
        "score_evidence",
        value!({"lens": "growth", "impact_weight": 2, "cost_weight": 1}),
    )?;
    let efficiency_result = efficiency.run(
        "score_evidence",
        value!({"lens": "efficiency", "impact_weight": 1, "cost_weight": 2}),
    )?;

    assert_eq!(
        growth_result.output,
        value!({"lens": "growth", "score": 23})
    );
    assert_eq!(
        efficiency_result.output,
        value!({"lens": "efficiency", "score": -5})
    );
    assert_eq!(parent.read("/memory/analysis")?, None);
    assert_eq!(
        growth.read("/memory/analysis/lens")?,
        Some(&Value::String("growth".into()))
    );
    assert_eq!(
        efficiency.read("/memory/analysis/lens")?,
        Some(&Value::String("efficiency".into()))
    );

    println!("fork_research parent=unchanged growth=23 efficiency=-5");
    Ok(())
}
