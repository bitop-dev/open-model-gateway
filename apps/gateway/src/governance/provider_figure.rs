//! A video charged its provider's reported cost (#52 part 2): OpenRouter
//! reports a video's cost and no duration, so that figure is the charge,
//! under the settlement rules below.
use super::settlement::Valuation;
use super::*;

/// The ledger evidence of a charge that is the provider's reported figure.
pub(super) const PROVIDER_REPORTED_COST: &str = "provider_reported_cost";

/// A video's charge when it is the provider's reported cost (OpenRouter
/// reports a cost and no duration): that figure, an estimate like every
/// other price, recorded as the video seconds' component. `free` says
/// whether the pinned price charges nothing for the admitted video
/// ([`charges_nothing`]): a cost of zero on a route that charges proves
/// nothing (a paid route does not make a free video), and an unknown cost
/// keeps the hold, never a zero. The rates' own valuation decides nothing
/// here (a request that named no size values as unpriced on a price by
/// size): only a figure above the hold is marked unbounded, by the caller,
/// as usage above a price's ceiling is.
pub(super) fn provider_figure(free: bool, cost: Option<i64>) -> Valuation {
    let unknown = Valuation {
        actual: None,
        floor: None,
        components: None,
        violated: false,
        provider_figure: false,
    };
    match cost {
        Some(cost) if cost > 0 || (cost == 0 && free) => Valuation {
            actual: Some(cost),
            floor: Some(cost),
            components: Some(CostBreakdown::Metered(
                billing::CostComponents::default(),
                billing::MeterCostComponents {
                    output_video_microusd: cost,
                    ..billing::MeterCostComponents::default()
                },
            )),
            violated: false,
            provider_figure: true,
        },
        _ => unknown,
    }
}

/// Whether the pinned price charges nothing for the admitted video at every
/// size it may have been made at: the size the request named, or, with none
/// named, every video-seconds line the price has (the provider reports no
/// size, so any of them may apply). Any rate, or a fee, that applies to one
/// of them means the route charges; a price without a video line charges
/// (it values nothing).
pub(super) fn charges_nothing(
    lines: &billing::v3::PriceLines,
    max: &billing::v3::MaxUnits,
    usage: Usage,
) -> Result<bool, billing::BillingError> {
    let video = |l: &&billing::v3::PriceLine| l.meter == billing::v3::Meter::OutputVideoSecondsMs;
    let variants: Vec<Option<billing::MeterVariant>> = match usage.output_image_variant {
        Some(named) => vec![Some(named)],
        None => lines.0.iter().filter(video).map(|l| l.variant).collect(),
    };
    if variants.is_empty() {
        return Ok(false);
    }
    // The video's length is not reported: a probe of one millisecond, which
    // a zero rate values at zero and any other rate above it (rounded up),
    // within any ceiling.
    let meters = usage.meters.map(|m| billing::MeterUsage {
        output_video_seconds_ms: Some(1),
        ..m
    });
    for variant in variants {
        let total = billing::v3::value(
            lines,
            max,
            &billing::v3::Observed {
                billing: usage.billing.as_ref(),
                output_tokens: usage.output_tokens,
                meters: meters.as_ref(),
                variant,
                audio: None,
                video: true,
            },
        )
        .and_then(|rated| {
            rated
                .components
                .map(|(c, m)| CostBreakdown::Metered(c, m).total())
                .transpose()
        });
        match total {
            Ok(Some(0)) => {}
            // A rate too large to value charges.
            Ok(_) | Err(billing::BillingError::Overflow) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
