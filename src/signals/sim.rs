//! `sim-signal://LINE`: a deterministic production line for trying signal
//! logging without hardware. Values depend only on the line and tick.
use super::{BrowseItem, Mode, Quality, Sample, TagInfo, Target, Value, helper::Ctx};
use anyhow::Result;
use std::time::Instant;

const BASE: [(&str, Option<&str>, &str); 6] = [
    ("temp_c", Some("degC"), "number"),
    ("pressure_bar", Some("bar"), "number"),
    ("count", None, "number"),
    ("running", None, "bool"),
    ("state", None, "text"),
    ("vibration", Some("mm/s"), "number"),
];

/// The line's tags: the six base tags, cut or extended with `aux_k`.
pub fn tags(line: &str, count: usize) -> Vec<TagInfo> {
    (0..count)
        .map(|i| match BASE.get(i) {
            Some((name, unit, kind)) => TagInfo {
                tag: format!("{line}/{name}"),
                unit: unit.map(str::to_string),
                kind: Some((*kind).into()),
            },
            None => TagInfo {
                tag: format!("{line}/aux_{}", i - BASE.len() + 1),
                unit: None,
                kind: Some("number".into()),
            },
        })
        .collect()
}

/// One tick's values.
pub fn values(line: &str, count: usize, tick: u64) -> Vec<Sample> {
    let t = tick as f64;
    let running = tick % 20 < 15;
    tags(line, count)
        .into_iter()
        .enumerate()
        .map(|(i, info)| {
            let value = match i {
                0 => Value::Number(20.0 + 5.0 * (t / 30.0).sin()),
                1 => Value::Number(2.0 + 0.1 * (tick % 10) as f64),
                2 => Value::Number(t),
                3 => Value::Bool(running),
                4 => Value::Text(if running { "run" } else { "idle" }.into()),
                5 => Value::Number(0.5 + 0.05 * (t / 7.0).cos()),
                k => Value::Number(((t + k as f64) / 10.0).sin()),
            };
            Sample::now(&info.tag, value, Quality::Good)
        })
        .collect()
}

pub fn run(ctx: &Ctx) -> Result<bool> {
    let Target::Sim { line, tags: count } = &ctx.uri.target else {
        unreachable!()
    };
    let infos = tags(line, *count);
    match ctx.mode() {
        Mode::Browse { limit, .. } => {
            let items: Vec<BrowseItem> = infos
                .iter()
                .zip(values(line, *count, 0))
                .map(|(info, sample)| BrowseItem {
                    id: info.tag.clone(),
                    name: info.tag.clone(),
                    kind: "tag".into(),
                    data_type: info.kind.clone(),
                    unit: info.unit.clone(),
                    value: Some(sample.value),
                })
                .collect();
            let truncated = items.len() > limit as usize;
            ctx.browse(items.into_iter().take(limit as usize).collect());
            Ok(truncated)
        }
        Mode::Read => {
            ctx.ready(infos);
            for sample in values(line, *count, 0) {
                ctx.sample(sample);
            }
            Ok(false)
        }
        Mode::Log => {
            ctx.ready(infos);
            ctx.connected();
            let mut tick_at = Instant::now();
            let mut tick = 0;
            loop {
                for sample in values(line, *count, tick) {
                    ctx.sample(sample);
                }
                ctx.flush_dropped();
                tick += 1;
                if !ctx.sleep_until(ctx.next_tick(&mut tick_at)) {
                    return Ok(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sized() {
        let a: Vec<_> = values("l1", 6, 7).into_iter().map(|s| s.value).collect();
        let b: Vec<_> = values("l1", 6, 7).into_iter().map(|s| s.value).collect();
        assert_eq!(a, b);
        assert_eq!(a[2], Value::Number(7.0));
        assert_eq!(a[3], Value::Bool(true));
        assert_eq!(values("l1", 6, 16)[4].value, Value::Text("idle".into()));
        assert_eq!(tags("l1", 2).len(), 2);
        let wide = tags("l1", 9);
        assert_eq!(wide[8].tag, "l1/aux_3");
        assert_eq!(values("l1", 64, 1).len(), 64);
    }
}
