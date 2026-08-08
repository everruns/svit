use svit::{Process, Value, value};

const SCORE_EVIDENCE: &str = r#"
(define (main input)
  (let ((impact-weight (value-get input "/impact_weight"))
        (cost-weight (value-get input "/cost_weight"))
        (evidence (read "/memory/evidence")))
    (let ((score (+ (- (* (value-get evidence "/0/impact") impact-weight)
                        (* (value-get evidence "/0/cost") cost-weight))
                    (- (* (value-get evidence "/1/impact") impact-weight)
                        (* (value-get evidence "/1/cost") cost-weight)))))
      (let ((analysis (value-map "lens" (value-get input "/lens") "score" score)))
        (do
          (write "/memory/analysis" analysis)
          analysis)))))
"#;

fn main() -> svit::Result<()> {
    let mut parent = Process::builder("svit://local/examples/research")?
        .memory(
            "evidence",
            value!([
                {"claim": "faster iteration", "impact": 8, "cost": 7},
                {"claim": "better isolation", "impact": 9, "cost": 4}
            ]),
        )
        .build()?;
    parent.write("/lib/score_evidence", value!({"source": SCORE_EVIDENCE}))?;

    let mut growth = parent.fork("svit://local/examples/research-growth")?;
    let mut efficiency = parent.fork("svit://local/examples/research-efficiency")?;

    let growth_result = growth.exec(
        "/lib/score_evidence",
        value!({"lens": "growth", "impact_weight": 2, "cost_weight": 1}),
    )?;
    let efficiency_result = efficiency.exec(
        "/lib/score_evidence",
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
