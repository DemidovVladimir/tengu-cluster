//! Tests of `src/domain/lp/hedge.rs`.

use super::*;

/// `baseInput` of `hedgeController.test.ts:8-34` — price pinned at $100.
fn base() -> HedgeInput {
    HedgeInput {
        lp_sol: 0.0,
        long_sol: 0.0,
        short_sol: 0.0,
        long_notional_usd: 0.0,
        short_notional_usd: 0.0,
        long_collateral_usd: 0.0,
        short_collateral_usd: 0.0,
        carry_cost_bps_long: 1200.0,
        carry_cost_bps_short: 1200.0,
        oracle_price_usd: Some(100.0),
        wallet_sol: 10.0,
        wallet_reserve_sol: 0.3,
        wallet_usdc: 1_000_000.0,
        target_delta_sol: 0.0,
        band_sol: 0.5,
        carry_cap_bps: 5000.0,
        max_hedge_notional_usd: 12_000.0,
        min_collateral_ratio: 0.15,
        target_collateral_ratio: 1.0,
        now_ms: 1_000_000,
        last_action_at_ms: None,
        cooldown_ms: 120_000,
    }
}

/// `toBeCloseTo(expected, digits)` — vitest: |a − b| < 10^−digits / 2.
fn close_to(a: f64, b: f64, digits: i32) -> bool {
    (a - b).abs() < 10f64.powi(-digits) / 2.0
}

fn reason(d: &Decision) -> &str {
    match d {
        Decision::None { reason } | Decision::Blocked { reason } => reason,
        _ => panic!("no reason on {d:?}"),
    }
}

// --- production vector replay (simulator/tests/vectors.rs) -------------

#[derive(Deserialize)]
struct Expected {
    action: String,
    size_usd: Option<f64>,
    entire_position: Option<bool>,
    withdraw_collateral_usd: Option<f64>,
    collateral_tokens: Option<f64>,
    adjust_sol: Option<f64>,
}

#[derive(Deserialize)]
struct Vector {
    input: HedgeInput,
    decision: Expected,
}

fn close(a: f64, b: f64) -> bool {
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= 1e-9 * scale
}

#[test]
fn rust_port_matches_all_production_vectors() {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hedge-vectors.jsonl"
    ));
    let mut checked = 0usize;
    for (line_no, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Vector =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {}: {e}", line_no + 1));
        let got = decide(&v.input);
        let ctx = format!(
            "vector at line {} (expected {})",
            line_no + 1,
            v.decision.action
        );
        assert_eq!(
            got.action(),
            v.decision.action,
            "{ctx}: got {}",
            got.action()
        );
        match &got {
            Decision::None { .. } | Decision::Blocked { .. } => {}
            Decision::DecreaseLong {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            }
            | Decision::DecreaseShort {
                size_usd,
                entire_position,
                withdraw_collateral_usd,
                adjust_sol,
            } => {
                assert!(
                    close(*size_usd, v.decision.size_usd.unwrap()),
                    "{ctx}: size {size_usd}"
                );
                assert_eq!(
                    *entire_position,
                    v.decision.entire_position.unwrap(),
                    "{ctx}: entire"
                );
                assert!(
                    close(
                        *withdraw_collateral_usd,
                        v.decision.withdraw_collateral_usd.unwrap()
                    ),
                    "{ctx}: withdraw {withdraw_collateral_usd}"
                );
                assert!(
                    close(*adjust_sol, v.decision.adjust_sol.unwrap()),
                    "{ctx}: adjust {adjust_sol}"
                );
            }
            Decision::IncreaseLong {
                size_usd,
                collateral_tokens,
                adjust_sol,
            }
            | Decision::IncreaseShort {
                size_usd,
                collateral_tokens,
                adjust_sol,
            } => {
                assert!(
                    close(*size_usd, v.decision.size_usd.unwrap()),
                    "{ctx}: size {size_usd}"
                );
                assert!(
                    close(*collateral_tokens, v.decision.collateral_tokens.unwrap()),
                    "{ctx}: collateral {collateral_tokens}"
                );
                assert!(
                    close(*adjust_sol, v.decision.adjust_sol.unwrap()),
                    "{ctx}: adjust {adjust_sol}"
                );
            }
        }
        checked += 1;
    }
    assert_eq!(
        checked, 1027,
        "fixture must carry all 1027 production vectors"
    );
}

// --- decideHedgeAction — band + cooldown gates -------------------------

#[test]
fn blocked_without_oracle_price() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        oracle_price_usd: Some(0.0),
        ..base()
    });
    assert_eq!(d.action(), "blocked");
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        oracle_price_usd: None,
        ..base()
    });
    assert_eq!(
        d,
        Decision::Blocked {
            reason: "no oracle SOL price available".into()
        }
    );
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        oracle_price_usd: Some(f64::NAN),
        ..base()
    });
    assert_eq!(d.action(), "blocked");
}

#[test]
fn none_within_band_and_at_edge() {
    let d = decide(&HedgeInput {
        lp_sol: 0.4,
        ..base()
    });
    assert_eq!(
        d,
        Decision::None {
            reason: "in band".into()
        }
    );
    let d = decide(&HedgeInput {
        lp_sol: 0.5,
        ..base()
    });
    assert_eq!(d.action(), "none");
}

#[test]
fn cooldown_suppresses_then_releases() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        last_action_at_ms: Some(1_000_000 - 30_000),
        ..base()
    });
    assert_eq!(
        d,
        Decision::None {
            reason: "cooldown: previous hedge request may still be filling (90000ms remaining)"
                .into()
        }
    );
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        last_action_at_ms: Some(1_000_000 - 120_001),
        ..base()
    });
    assert_eq!(d.action(), "increase_short");
    // In-band reads report none (not cooldown) even while cooling down.
    let d = decide(&HedgeInput {
        lp_sol: 0.1,
        last_action_at_ms: Some(1_000_000 - 1_000),
        ..base()
    });
    assert_eq!(
        d,
        Decision::None {
            reason: "in band".into()
        }
    );
}

// --- reduce delta (error > band) ---------------------------------------

#[test]
fn increases_short_sized_to_full_error() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        ..base()
    });
    assert_eq!(
        d,
        Decision::IncreaseShort {
            size_usd: 500.0,
            collateral_tokens: 500.0,
            adjust_sol: -5.0
        }
    );
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        target_collateral_ratio: 0.33,
        ..base()
    });
    let Decision::IncreaseShort {
        collateral_tokens, ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(close_to(collateral_tokens, 165.0, 6));
}

#[test]
fn decreases_open_long_first() {
    let d = decide(&HedgeInput {
        lp_sol: 1.0,
        long_sol: 5.0,
        long_notional_usd: 500.0,
        long_collateral_usd: 500.0,
        ..base()
    });
    let Decision::DecreaseLong {
        entire_position,
        adjust_sol,
        withdraw_collateral_usd,
        ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(entire_position);
    assert_eq!(adjust_sol, -5.0);
    assert_eq!(withdraw_collateral_usd, 0.0);
}

#[test]
fn partially_decreases_long() {
    let d = decide(&HedgeInput {
        long_sol: 5.0,
        long_notional_usd: 500.0,
        long_collateral_usd: 500.0,
        target_delta_sol: 3.0,
        ..base()
    });
    let Decision::DecreaseLong {
        entire_position,
        size_usd,
        withdraw_collateral_usd,
        adjust_sol,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(!entire_position);
    assert!(close_to(size_usd, 200.0, 6));
    assert!(close_to(withdraw_collateral_usd, 200.0, 6));
    assert!(close_to(adjust_sol, -2.0, 9));
}

#[test]
fn residual_within_epsilon_is_full_close() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0 - 1e-12,
        long_sol: 5.0,
        long_notional_usd: 500.0,
        target_delta_sol: 5.0,
        ..base()
    });
    let Decision::DecreaseLong {
        entire_position, ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(entire_position);
}

// --- add delta (error < −band) -----------------------------------------

#[test]
fn decreases_open_short_first() {
    let d = decide(&HedgeInput {
        lp_sol: 1.0,
        short_sol: 3.0,
        short_notional_usd: 300.0,
        short_collateral_usd: 300.0,
        ..base()
    });
    let Decision::DecreaseShort {
        entire_position,
        size_usd,
        adjust_sol,
        ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(!entire_position);
    assert!(close_to(size_usd, 200.0, 6));
    assert!(close_to(adjust_sol, 2.0, 9));

    let d = decide(&HedgeInput {
        short_sol: 2.0,
        short_notional_usd: 200.0,
        ..base()
    });
    let Decision::DecreaseShort {
        entire_position,
        adjust_sol,
        ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(entire_position);
    assert!(close_to(adjust_sol, 2.0, 9));
}

#[test]
fn opens_long_when_flat() {
    let d = decide(&HedgeInput {
        target_delta_sol: 5.0,
        ..base()
    });
    let Decision::IncreaseLong {
        size_usd,
        collateral_tokens,
        adjust_sol,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(close_to(size_usd, 500.0, 6));
    assert!(close_to(collateral_tokens, 5.0, 9));
    assert!(close_to(adjust_sol, 5.0, 9));
}

#[test]
fn fills_long_up_to_sol_above_reserves() {
    let d = decide(&HedgeInput {
        target_delta_sol: 5.0,
        wallet_sol: 5.0,
        wallet_reserve_sol: 0.3,
        ..base()
    });
    let Decision::IncreaseLong {
        size_usd,
        collateral_tokens,
        adjust_sol,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(close_to(size_usd, 470.0, 6));
    assert!(close_to(collateral_tokens, 4.7, 9));
    assert!(close_to(adjust_sol, 4.7, 9));
}

#[test]
fn blocks_long_when_reserves_afford_too_little() {
    let d = decide(&HedgeInput {
        target_delta_sol: 5.0,
        wallet_sol: 0.35,
        wallet_reserve_sol: 0.3,
        ..base()
    });
    assert_eq!(d.action(), "blocked");
    assert!(reason(&d).contains("reserves"), "{d:?}");
    assert_eq!(
            reason(&d),
            "long collateral 5.0000 SOL exceeds available wallet SOL 0.0500 above reserves and the affordable size $5.00 is below the $10 minimum increase"
        );
}

// --- increase guards ---------------------------------------------------

#[test]
fn carry_gate_per_side_and_disabled_at_zero() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        carry_cost_bps_long: 0.0,
        carry_cost_bps_short: 6000.0,
        ..base()
    });
    assert_eq!(reason(&d), "short carry 60.00% APR exceeds cap 50.00%");
    let d = decide(&HedgeInput {
        target_delta_sol: 5.0,
        carry_cost_bps_long: 6000.0,
        carry_cost_bps_short: 0.0,
        ..base()
    });
    assert!(reason(&d).contains("long carry"), "{d:?}");
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        carry_cap_bps: 0.0,
        carry_cost_bps_long: 99999.0,
        carry_cost_bps_short: 99999.0,
        ..base()
    });
    assert_eq!(d.action(), "increase_short");
}

#[test]
fn fills_remaining_cap_headroom() {
    let d = decide(&HedgeInput {
        lp_sol: 130.0,
        ..base()
    });
    let Decision::IncreaseShort {
        size_usd,
        collateral_tokens,
        adjust_sol,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(close_to(size_usd, 12_000.0, 6));
    assert!(close_to(adjust_sol, -120.0, 9));
    assert!(close_to(collateral_tokens, 12_000.0, 6));
}

#[test]
fn fills_short_up_to_wallet_usdc() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        wallet_usdc: 50.0,
        ..base()
    });
    let Decision::IncreaseShort {
        size_usd,
        collateral_tokens,
        adjust_sol,
    } = d
    else {
        panic!("{d:?}")
    };
    assert!(close_to(size_usd, 50.0, 6));
    assert!(close_to(collateral_tokens, 50.0, 6));
    assert!(close_to(adjust_sol, -0.5, 9));
}

#[test]
fn blocks_short_when_usdc_affords_too_little() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        wallet_usdc: 5.0,
        ..base()
    });
    assert!(reason(&d).contains("USDC"), "{d:?}");
}

#[test]
fn blocks_when_headroom_below_minimum() {
    let d = decide(&HedgeInput {
        lp_sol: 130.0,
        short_sol: 119.95,
        short_notional_usd: 11_995.0,
        short_collateral_usd: 11_995.0,
        ..base()
    });
    assert!(reason(&d).contains("headroom"), "{d:?}");
}

#[test]
fn blocks_when_projected_ratio_below_floor() {
    let d = decide(&HedgeInput {
        lp_sol: 5.0,
        target_collateral_ratio: 0.1,
        ..base()
    });
    assert_eq!(
        reason(&d),
        "projected collateral ratio 0.100 below min 0.15"
    );
}

#[test]
fn never_blocks_a_decrease_on_carry() {
    let d = decide(&HedgeInput {
        short_sol: 2.0,
        short_notional_usd: 200.0,
        carry_cost_bps_long: 99999.0,
        carry_cost_bps_short: 99999.0,
        ..base()
    });
    assert_eq!(d.action(), "decrease_short");
}

#[test]
fn both_sides_open_reduces_opposing_side_first() {
    let d = decide(&HedgeInput {
        long_sol: 2.0,
        short_sol: 1.0,
        long_notional_usd: 200.0,
        short_notional_usd: 100.0,
        ..base()
    });
    assert_eq!(d.action(), "decrease_long");
}

#[test]
fn decision_serialises_with_action_tag() {
    let d = Decision::IncreaseShort {
        size_usd: 500.0,
        collateral_tokens: 500.0,
        adjust_sol: -5.0,
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"action": "increase_short", "size_usd": 500.0, "collateral_tokens": 500.0, "adjust_sol": -5.0})
    );
    assert_eq!(serde_json::from_value::<Decision>(v).unwrap(), d);
    assert_eq!(serde_json::to_value(LpRegime::Below).unwrap(), "below");
}

// --- LP transforms -----------------------------------------------------

#[test]
fn midpoint_sol() {
    assert!(close_to(lp_midpoint_sol(0.61, 50.02, 82.0), 0.61, 2));
    assert!(close_to(lp_midpoint_sol(1.22, 0.0, 82.0), 0.61, 2));
    assert!(close_to(lp_midpoint_sol(0.0, 100.04, 82.0), 0.61, 2));
    assert_eq!(lp_midpoint_sol(0.0, 0.0, 82.0), 0.0);
    assert_eq!(lp_midpoint_sol(0.7, 50.0, 0.0), 0.7);
    assert_eq!(lp_midpoint_sol(0.7, 50.0, f64::NAN), 0.7);
}

#[test]
fn hedge_delta_regimes_and_hysteresis() {
    let r = lp_hedge_delta(0.61, 50.02, 82.0, LpRegime::In);
    assert_eq!(r.regime, LpRegime::In);
    assert!(close_to(r.delta_sol, 0.61, 2));
    let r = lp_hedge_delta(1.22, 0.0, 82.0, LpRegime::In);
    assert_eq!(r.regime, LpRegime::Below);
    assert!(close_to(r.delta_sol, 1.22, 9));
    let r = lp_hedge_delta(0.0, 100.04, 82.0, LpRegime::In);
    assert_eq!(r.regime, LpRegime::Above);
    assert_eq!(r.delta_sol, 0.0);
    // 94% SOL: not enough to enter fresh, keeps an existing clamp.
    assert_eq!(
        lp_hedge_delta(1.15, 6.0, 82.0, LpRegime::In).regime,
        LpRegime::In
    );
    assert_eq!(
        lp_hedge_delta(1.15, 6.0, 82.0, LpRegime::Below).regime,
        LpRegime::Below
    );
    // 85% SOL releases it.
    assert_eq!(
        lp_hedge_delta(1.0, 14.5, 82.0, LpRegime::Below).regime,
        LpRegime::In
    );
    assert_eq!(lp_hedge_delta(0.0, 0.0, 82.0, LpRegime::In).delta_sol, 0.0);
    assert_eq!(lp_hedge_delta(0.7, 50.0, 0.0, LpRegime::In).delta_sol, 0.7);
    // Mirror hysteresis above: 5% SOL keeps `above`, fresh stays `in`.
    let (sol, usdc) = (0.05, 95.0 * 82.0 / 100.0 * 1.0);
    assert_eq!(
        lp_hedge_regime(sol, usdc, 82.0, LpRegime::Above),
        LpRegime::Above
    );
    assert_eq!(lp_hedge_regime(sol, usdc, 82.0, LpRegime::In), LpRegime::In);
}

#[test]
fn delta_for_regime() {
    assert_eq!(lp_delta_for_regime(LpRegime::Below, 1.22, 3.0, 82.0), 1.22);
    assert_eq!(lp_delta_for_regime(LpRegime::Above, 1.22, 3.0, 82.0), 0.0);
    assert!(close_to(
        lp_delta_for_regime(LpRegime::In, 0.61, 50.02, 82.0),
        (0.61 + 50.02 / 82.0) / 2.0,
        9
    ));
}

#[test]
fn ramp_delta_endpoints() {
    // share 1.0 → full bag (= the ADR-021 clamp); healthy middle → midpoint.
    assert_eq!(lp_ramp_delta(1.22, 0.0, 82.0, 0.9), 1.22);
    assert!(close_to(
        lp_ramp_delta(0.61, 50.02, 82.0, 0.9),
        lp_midpoint_sol(0.61, 50.02, 82.0),
        12
    ));
    assert_eq!(lp_ramp_delta(0.0, 100.0, 82.0, 0.9), 0.0);
}

#[test]
fn auto_notional_cap() {
    assert!(close_to(
        auto_notional_cap_usd(2.63, 80.5, 1.25, 0.0),
        264.64,
        1
    ));
    assert_eq!(auto_notional_cap_usd(2.63, 80.5, 1.25, 200.0), 200.0);
    assert!(close_to(
        auto_notional_cap_usd(2.63, 80.5, 1.25, 10_000.0),
        264.64,
        1
    ));
    assert_eq!(auto_notional_cap_usd(0.0, 80.5, 1.25, 500.0), 500.0);
    assert_eq!(auto_notional_cap_usd(0.0, 80.5, 1.25, 0.0), 0.0);
    assert_eq!(auto_notional_cap_usd(2.63, f64::NAN, 1.25, 500.0), 500.0);
}

#[test]
fn auto_band() {
    assert_eq!(auto_band_sol(99.23 / 81.32, 20.0, 4.0, 0.25), 0.25);
    assert!(close_to(
        auto_band_sol(300.0 / 81.32, 20.0, 4.0, 0.25),
        0.7379,
        3
    ));
    assert_eq!(auto_band_sol(1.22, 20.0, 0.0, 0.25), 0.25);
    assert_eq!(auto_band_sol(0.0, 20.0, 4.0, 0.25), 0.25);
    assert_eq!(auto_band_sol(f64::NAN, 20.0, 4.0, 0.25), 0.25);
    assert_eq!(auto_band_sol(1.22, 0.0, 4.0, 0.25), 0.25);
}

#[test]
fn js_to_fixed_matches_v8() {
    // Expected strings produced by node v24 `x.toFixed(d)`.
    let cases: &[(f64, [&str; 4])] = &[
        (0.125, ["0", "0.1", "0.13", "0.125"]),
        (1.005, ["1", "1.0", "1.00", "1.005"]),
        (2.5, ["3", "2.5", "2.50", "2.500"]),
        (-1.5, ["-2", "-1.5", "-1.50", "-1.500"]),
        (1234.5678, ["1235", "1234.6", "1234.57", "1234.568"]),
        (0.0, ["0", "0.0", "0.00", "0.000"]),
        (-0.001, ["-0", "-0.0", "-0.00", "-0.001"]),
        (0.5, ["1", "0.5", "0.50", "0.500"]),
        (1.45, ["1", "1.4", "1.45", "1.450"]),
        (8.345, ["8", "8.3", "8.35", "8.345"]),
        (99.995, ["100", "100.0", "100.00", "99.995"]),
        (0.045, ["0", "0.0", "0.04", "0.045"]),
    ];
    for (x, want) in cases {
        for (d, w) in want.iter().enumerate() {
            assert_eq!(js_to_fixed(*x, d), *w, "({x}).toFixed({d})");
        }
    }
    assert_eq!(js_to_fixed(f64::NAN, 2), "NaN");
    assert_eq!(js_to_fixed(f64::INFINITY, 2), "Infinity");
}
