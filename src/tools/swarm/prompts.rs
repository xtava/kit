use super::model::{DevilOutput, ExpertOutput, ExpertRole, SynthesisOutput};

pub fn planner(original: &str) -> String {
    format!(
        "STAGE: planning\nAGENT_ID: planner\n\
         You are the deterministic planner for a council of independent experts. Read the task, but \
         do not answer it. Define 3 to 5 genuinely conflicting expert roles. Each role needs a \
         precise title, mandate, and perspective. Avoid overlapping roles.\n\nORIGINAL_TASK:\n{original}"
    )
}

pub fn expert(original: &str, agent: &str, role: &ExpertRole) -> String {
    format!(
        "STAGE: experts\nAGENT_ID: {agent}\n\
         You are an isolated expert. Produce your own analysis without assuming or predicting what \
         any peer will say. Follow only the role below and the original task.\n\nROLE:\n{}\n\n\
         MANDATE:\n{}\n\nPERSPECTIVE:\n{}\n\nORIGINAL_TASK:\n{original}",
        role.title, role.mandate, role.perspective
    )
}

pub fn rebuttal(
    original: &str,
    agent: &str,
    role: &ExpertRole,
    own: &ExpertOutput,
    peers: &[(String, ExpertRole, ExpertOutput)],
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "STAGE: debate\nAGENT_ID: {agent}\n\
         Continue your exact existing thread. Stress-test your first analysis against every peer \
         record. State what you accept, what you reject, and a revised recommendation.\n\n\
         ORIGINAL_TASK:\n{original}\n\nROLE:\n{}\n\nOWN_FIRST_PASS:\n{}\n\nPEER_RECORDS:\n{}",
        role.title,
        json(own)?,
        json(peers)?
    ))
}

pub fn devil<T: serde::Serialize>(
    original: &str,
    records: &T,
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "STAGE: devil\nAGENT_ID: devil\n\
         You are a new adversarial reviewer. Attack the hardened expert records below. Identify the \
         strongest objections, concrete failure modes, and corrections required before synthesis.\n\n\
         ORIGINAL_TASK:\n{original}\n\nHARDENED_RECORDS:\n{}",
        json(records)?
    ))
}

pub fn synthesis<T: serde::Serialize>(
    original: &str,
    ordered_records: &T,
    devil: &DevilOutput,
) -> Result<String, serde_json::Error> {
    Ok(format!(
        "STAGE: synthesis\nAGENT_ID: synthesis\n\
         Produce the final answer to the original task. Preserve meaningful dissent, resolve the \
         devil's required corrections, and state calibrated confidence. Expert records are in \
         planner order and must remain in that order.\n\nORIGINAL_TASK:\n{original}\n\n\
         ORDERED_EXPERT_RECORDS:\n{}\n\nDEVIL_REVIEW:\n{}",
        json(ordered_records)?,
        json(devil)?
    ))
}

pub fn report(result: &SynthesisOutput) -> String {
    result.answer.clone()
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}
