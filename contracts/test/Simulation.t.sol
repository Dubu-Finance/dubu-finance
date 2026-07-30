// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {console2} from "forge-std/console2.sol";

import {FlowModel} from "../script/lib/FlowModel.sol";

import {PropPool} from "../src/PropPool.sol";
import {IPropPool} from "../src/interfaces/IPropPool.sol";
import {PropCurve} from "../src/libraries/PropCurve.sol";
import {MockERC20} from "../src/mocks/MockERC20.sol";

contract SimulationTest is Test {

    MockERC20 internal baseToken;
    MockERC20 internal quoteToken;
    PropPool internal pool;

    address internal owner = makeAddr("owner");
    address internal manager = makeAddr("manager");
    address internal updater = makeAddr("updater");
    address internal guardian = makeAddr("guardian");
    address internal taker = makeAddr("taker");

    uint16 internal constant PAIR_ID = 1;

    uint8 internal constant PRICE_SCALE_EXP = 24;
    uint256 internal constant SCALE = 1e24;

    uint256 internal constant REF_MID = 2_000 * 1e12;

    uint256 internal constant TVL_BASE = 5_000e18;
    uint256 internal constant TVL_QUOTE = 10_000_000e6;

    uint32 internal constant MAX_STALE_SECS = 3_600;
    uint256 internal constant T0 = 1_800_000_000;

    uint96 internal constant CAPACITY = 1_000e18;

    struct RunState {

        uint256 pendingMid;

        uint256 pendingLandsAt;
        bool pendingRefresh;
        uint256 lastPushAt;

        uint256 withdrawLandsAt;

        uint256 cooloffUntil;
        uint256 jumpsDetected;

        uint256 fillsWhileWithdrawing;
        uint256 arrivalsRefusedInCooloff;

        uint256 ticksPerSec;
        uint256 quoteLatencyTicks;
        uint256 withdrawLatencyTicks;
        uint256 cooloffTicks;
        uint256 heartbeatTicks;
        uint256 decisionTicks;
        uint256 detectorTicks;

        uint256 prevQuote;
        uint256 prevBase;
        uint256 prevFair;
        bool navSeeded;
    }

    RunState internal S;

    function _defaults() internal view returns (FlowModel.Params memory p) {
        p.mid = REF_MID;
        p.priceScaleExp = PRICE_SCALE_EXP;
        p.baseDecimals = 18;
        p.quoteDecimals = 6;
        p.horizonSecs = 3_600;
        p.seed = 20_260_727;
        p.tickMs = vm.envOr("TICK_MS", uint256(200));

        p.sigmaE2 = 100;
        p.driftE2 = 0;
        p.jumpAtSecs = 0;
        p.jumpE2 = 0;

        p.halfSpreadBps = 5;
        p.widthBps = 25;
        p.bidCapacity = CAPACITY;
        p.askCapacity = CAPACITY;
        p.quoteLatencyMs = 2_000;
        p.decisionIntervalMs = 1_000;
        p.detectorIntervalMs = 200;
        p.adverseDriftE2 = 50;
        p.favourableDriftE2 = 800;
        p.heartbeatSecs = 2_400;
        p.capacityDivergencePct = 30;

        p.uninformedPerHour = 60;
        p.informedMaxPerSecE6 = 1_000_000;
        p.informedHalfMaxE2 = 1_000;
        p.informedCostE2 = 100;
        p.informedCapturePct = 100;
        p.fastHitPct = 50;

        p.s1E2 = 0;
        p.halfSpreadCapBps = 0;
        p.jumpThresholdE2 = 0;
        p.cooloffSecs = 0;
        p.withdrawLatencyMs = 0;
        p.decaySecs = 0;
    }

    function _defended() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.s1E2 = vm.envOr("S1_E2", uint256(50));
        p.halfSpreadCapBps = vm.envOr("SPREAD_CAP_BPS", uint256(50));
        p.jumpThresholdE2 = vm.envOr("JUMP_BPS", uint256(25)) * 100;
        p.cooloffSecs = vm.envOr("COOLOFF", uint256(30));
        p.withdrawLatencyMs = vm.envOr("WITHDRAW_LAT_MS", uint256(2_000));
        p.decaySecs = uint16(vm.envOr("DECAY_SECS", uint256(60)));
    }

    function _flat() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.sigmaE2 = 0;
    }

    function _jump() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.jumpAtSecs = p.horizonSecs / 2;
        p.jumpE2 = 10_000;
    }

    function _trend() internal view returns (FlowModel.Params memory p) {
        p = _defaults();
        p.driftE2 = 20;
    }

    function _jumpDefended() internal view returns (FlowModel.Params memory p) {
        p = _defended();
        p.jumpAtSecs = p.horizonSecs / 2;

        p.jumpE2 = int256(vm.envOr("JUMP_SIZE_BPS", uint256(100))) * 100;
    }

    function test_defences_beforeAndAfter() public {
        FlowModel.Params memory before = _jump();
        FlowModel.printParams(before, "JUMP - BEFORE, no defences");
        FlowModel.Result memory a = _run(before);
        _report(a, before);
        int256 pnlBefore = _totalPnl(a);

        FlowModel.Params memory p = _jumpDefended();
        FlowModel.printParams(p, "JUMP - AFTER: vol-scaled spread, jump withdrawal, capacity ramp");
        console2.log(
            string.concat(
                "  defences: s1=",
                FlowModel.u2s(p.s1E2),
                "e-2  cap=",
                FlowModel.u2s(p.halfSpreadCapBps),
                "bp  jumpTrip=",
                FlowModel.u2s(p.jumpThresholdE2 / 100),
                "bp  cooloff=",
                FlowModel.u2s(p.cooloffSecs),
                "s  withdrawLat=",
                FlowModel.u2s(p.withdrawLatencyMs),
                "ms  decay=",
                FlowModel.u2s(p.decaySecs),
                "s (quantised to blocks)"
            )
        );
        FlowModel.Result memory b = _run(p);
        _report(b, p);
        int256 pnlAfter = _totalPnl(b);

        console2.log("");
        console2.log("=== BEFORE vs AFTER =========================================");
        console2.log(
            string.concat(
                "  informed fills      ",
                FlowModel.u2s(a.byPop[FlowModel.POP_INFORMED].fills),
                " -> ",
                FlowModel.u2s(b.byPop[FlowModel.POP_INFORMED].fills)
            )
        );
        console2.log(
            string.concat(
                "  uninformed fills    ",
                FlowModel.u2s(a.byPop[FlowModel.POP_UNINFORMED].fills),
                " -> ",
                FlowModel.u2s(b.byPop[FlowModel.POP_UNINFORMED].fills)
            )
        );
        console2.log(
            string.concat(
                "  declined (refused)  ", FlowModel.u2s(a.declinedFills), " -> ", FlowModel.u2s(b.declinedFills)
            )
        );
        console2.log(string.concat("  jumps detected      0 -> ", FlowModel.u2s(S.jumpsDetected)));
        console2.log(string.concat("  fills while withdrawing  ", FlowModel.u2s(S.fillsWhileWithdrawing)));
        console2.log("  total PnL (quote units, signed):");
        console2.logInt(pnlBefore);
        console2.logInt(pnlAfter);
        console2.log("=============================================================");

        assertLe(pnlBefore, pnlAfter, "the defences made it worse, which is a bug and not a trade-off");
    }

    function _totalPnl(FlowModel.Result memory res) internal pure returns (int256 total) {
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            total += res.byPop[i].spread;
        }
    }

    function test_defenceIsolation() public {
        _sweepHeader(
            "DEFENCE ISOLATION", "defence", "3600s horizon, +100 bp jump at t=1800, $2M epoch, one defence at a time."
        );
        for (uint256 i; i < 5; ++i) {
            FlowModel.Params memory p = _isolate(i);
            FlowModel.Result memory res = _run(p);
            _sweepRow(_isolateLabel(i), "", res, p);
        }
        console2.log("  The spread($) column is the number to read across the two clocks: it is the P&L");
        console2.log("  booked at fill time, unpolluted by the 60s of diffusion that follows a fill.");
        console2.log("");
        console2.log("  ONE SEED PER ROW, so a row that moves by less than a thousand dollars has not");
        console2.log("  moved. Two of these three defences are worth about that much, which is the point:");
        console2.log("");
        console2.log("  * The capacity ramp is arithmetically incapable of more. It haircuts depth by");
        console2.log("    age/decaySecs, the bot requotes every ~2s, and 2/60 is a 3.3% ceiling on a 3.3%");
        console2.log("    ceiling on a $16k pick-off -- about $520 at the very most, and only if the gap");
        console2.log("    lands at the oldest moment of the cycle. Whether a given run gets 0% or 1.7% is");
        console2.log("    decided by whether the pick-off tick coincides with a push landing, which is");
        console2.log("    phase, not defence. DECAY_SECS=3 makes it bite properly (-$11,213 here, a $4,400");
        console2.log("    saving) and is a different, much more expensive trade: it refuses depth to");
        console2.log("    everyone, all the time, to be ready for a gap that arrives once an hour.");
        console2.log("  * The jump withdrawal does nothing at a 2,000ms withdrawal latency, at either");
        console2.log("    clock, because it loses a race it starts level in -- the detector and the");
        console2.log("    searcher both fire on the tick the gap prints. Sub-second is the only version of");
        console2.log("    it that works; test_theWithdrawalRaceAcrossTheSecondBoundary is that measurement");
        console2.log("    and it is the one thing in this file the finer clock changed the answer to.");
        console2.log("  * The vol-scaled spread is the only row that moves for a reason, and it moves the");
        console2.log("    same ~$1,600 at both clocks -- it raises the absorption limit, which is a");
        console2.log("    property of the ladder and has nothing to do with time at all.");
    }

    function _isolate(uint256 i) internal view returns (FlowModel.Params memory p) {
        p = _jumpDefended();
        if (i != 1 && i != 4) {
            p.s1E2 = 0;
            p.halfSpreadCapBps = 0;
        }
        if (i != 2 && i != 4) {
            p.jumpThresholdE2 = 0;
            p.cooloffSecs = 0;
            p.withdrawLatencyMs = 0;
        }
        if (i != 3 && i != 4) p.decaySecs = 0;
    }

    function _isolateLabel(uint256 i) internal pure returns (string memory) {
        if (i == 0) return "control (none)";
        if (i == 1) return "vol-scaled spread";
        if (i == 2) return "jump withdrawal";
        if (i == 3) return "capacity ramp";
        return "all three";
    }

    function test_theOneSecondControlReproducesTheOldModel() public {
        FlowModel.Params memory bare = _jump();
        bare.tickMs = 1_000;
        FlowModel.Result memory a = _run(bare);

        FlowModel.Params memory defended = _jumpDefended();
        defended.tickMs = 1_000;
        FlowModel.Result memory b = _run(defended);

        console2.log("");
        FlowModel.rule();
        console2.log(" ONE-SECOND CONTROL: this model at tickMs=1000 vs the model before it had ticks");
        FlowModel.rule();
        console2.log(
            string.concat("  no defences, expected -15757.945680  got ", FlowModel.signedUnits(_totalPnl(a), 6))
        );
        console2.log(
            string.concat("  all defences, expected -13940.431511  got ", FlowModel.signedUnits(_totalPnl(b), 6))
        );
        FlowModel.rule();

        assertEq(_totalPnl(a), -15_757_945_680, "the 1s control no longer reproduces the pre-tick model (undefended)");
        assertEq(_totalPnl(b), -13_940_431_511, "the 1s control no longer reproduces the pre-tick model (defended)");
        assertEq(a.ticksRun, 3_600, "a 3600s horizon at a 1000ms tick is 3600 ticks");
        assertEq(a.blockSecsAdvanced, 3_600, "the block clock did not advance once per tick at tickMs=1000");
    }

    function test_simulation_flatReference() public {
        FlowModel.Params memory p = _flat();
        FlowModel.printParams(p, "FLAT - the reference never moves");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        assertEq(res.byPop[FlowModel.POP_INFORMED].fills, 0, "a correct quote cannot be picked off");
        assertEq(res.byPop[FlowModel.POP_FAST].fills, 0, "nor by a faster taker");
        assertGt(res.byPop[FlowModel.POP_UNINFORMED].fills, 0, "the control run traded nothing");

        FlowModel.Accum memory u = res.byPop[FlowModel.POP_UNINFORMED];
        assertEq(u.mo60, u.spread, "a flat reference must mark out at exactly the spread");

        int256 bps = FlowModel.bpsOf(u.spread, u.notional);
        assertGt(bps, 490, "uninformed flow paid less than the half-spread");
        assertLt(bps, 700, "uninformed flow paid implausibly more than the half-spread");
    }

    function test_theLatencyAdvantagedTakerIsTheWorstCase() public {
        FlowModel.Params memory p = _jump();
        p.horizonSecs = 900;
        p.jumpAtSecs = 450;
        p.informedMaxPerSecE6 = 0;
        p.fastHitPct = 100;

        FlowModel.printParams(p, "JUMP, with ONLY the latency-advantaged taker enabled");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        assertEq(res.byPop[FlowModel.POP_INFORMED].fills, 0, "the ordinary informed taker was meant to be off");
        assertGt(res.byPop[FlowModel.POP_FAST].fills, 0, "the latency-advantaged taker never got in");
        assertLt(res.byPop[FlowModel.POP_FAST].mo60, 0, "a pick-off that made the pool money is not a pick-off");
    }

    function test_simulation_diffusionOnly() public {
        FlowModel.Params memory p = _defaults();
        FlowModel.printParams(p, "DIFFUSION - 1 bp/s random walk, no jump");
        FlowModel.Result memory res = _run(p);
        _report(res, p);
    }

    function test_simulation_jumpPath() public {
        FlowModel.Params memory p = _jump();
        FlowModel.printParams(p, "JUMP - diffusion plus +100 bp in a single second");
        FlowModel.Result memory res = _run(p);
        _report(res, p);

        console2.log("");
        console2.log("  The jump in one line: the reference gaps 100 bp while the pool's ladder is 2s old.");
        console2.log("  Whoever gets there first buys the pool's entire ask epoch at the pre-jump price.");
        console2.log("  Nothing about the half-spread bounds that loss. The per-epoch CAPACITY does, and");
        console2.log("  it is the only thing that does -- which is what archi_v2 5.4 means by a risk budget.");
        _printAbsorptionRule(p);
    }

    function _printAbsorptionRule(FlowModel.Params memory p) internal pure {
        uint256 absorbE2 = FlowModel.halfSpreadAt(p) * 100 + (p.widthBps * 100) / 2;
        console2.log("");
        console2.log(
            string.concat(
                "  ABSORPTION LIMIT = halfSpread + width/2 = ",

                FlowModel.sbps(int256(absorbE2)),
                " bp at this ladder."
            )
        );
        console2.log("  A reference error below that is absorbed and the pool still profits. Above it the");
        console2.log("  pool pays the excess on every unit of depth the epoch had posted. Two levers move");
        console2.log("  the limit -- half-spread and width -- and width moves it at half the rate.");
    }

    function test_simulation_trendPath() public {
        FlowModel.Params memory p = _trend();
        FlowModel.printParams(p, "TREND - +0.20 bp/s, a 7.2% one-way hour");
        FlowModel.Result memory res = _run(p);
        _report(res, p);
    }

    function test_sweep_capacityIsTheRiskBudget() public {
        uint96[5] memory caps = [uint96(1_000e18), uint96(250e18), uint96(50e18), uint96(10e18), uint96(2e18)];
        _sweepHeader("PER-EPOCH CAPACITY", "capacity          $/epoch");
        for (uint256 i; i < caps.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.bidCapacity = caps[i];
            p.askCapacity = caps[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(
                string.concat(FlowModel.u2s(uint256(caps[i]) / 1e18), " base"),
                string.concat("$", FlowModel.thousands(((uint256(caps[i]) * REF_MID) / SCALE) / 1e6)),
                res,
                p
            );
        }
        console2.log("  Same 100 bp jump, same flow, only the risk budget changes. The loss tracks capacity");
        console2.log("  almost linearly, because the pick-off clears whatever depth is exposed and the size");
        console2.log("  of the error decides the price, not the size of the trade. THIS is the lever: the");
        console2.log("  live configuration posts $2M an epoch, and a single 100 bp gap against it is worth");
        console2.log("  five figures to whoever is watching. archi_v2 5.4 calls capacity a risk budget; this");
        console2.log("  is what that sentence costs when the number is set by how much volume you want.");
    }

    function test_sweep_theThreeLeversTogether() public {
        uint256[3] memory s1s = [uint256(58), 30, 0];
        uint96[3] memory caps = [uint96(1_000e18), uint96(250e18), uint96(50e18)];
        uint256[2] memory widths = [uint256(25), 8];

        _sweepHeader("s1 x CAPACITY x WIDTH", "s1 / epoch / width");
        for (uint256 a; a < s1s.length; ++a) {
            for (uint256 b; b < caps.length; ++b) {
                for (uint256 c; c < widths.length; ++c) {
                    FlowModel.Params memory p = _jump();
                    p.horizonSecs = 900;
                    p.jumpAtSecs = 450;
                    p.s1E2 = s1s[a];
                    p.halfSpreadBps = 1;
                    p.bidCapacity = caps[b];
                    p.askCapacity = caps[b];
                    p.widthBps = widths[c];
                    FlowModel.Result memory res = _run(p);
                    _sweepRow(
                        string.concat(
                            "s1=", FlowModel.u2s(s1s[a]),
                            " cap=", FlowModel.u2s(uint256(caps[b]) / 1e18),
                            " w=", FlowModel.u2s(widths[c])
                        ),
                        string.concat("$", FlowModel.thousands(((uint256(caps[b]) * REF_MID) / SCALE) / 1e6)),
                        res,
                        p
                    );
                }
            }
        }
        console2.log("  s0 is 1 bp on every row, so the posted half-spread is 1 + s1 * sigma and the s1 column");
        console2.log("  is the whole difference between quoting near the floor and quoting near the old 7.5.");
        console2.log("  Read the capacity column first: if it dominates, the spread is not what was protecting");
        console2.log("  us and the tightening is affordable. If s1 dominates instead, it is not, and the");
        console2.log("  standing objection survives its own re-measurement.");
    }

    function test_sweep_quoteLatencyAcrossTheSecondBoundary() public {
        uint256[5] memory latMs = [uint256(200), 400, 1_000, 2_000, 5_000];

        _latencyHeader("LANDING LATENCY ONLY -- 1 Hz newHeads polling held FIXED", LATENCY_SEEDS);
        for (uint256 i; i < latMs.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], 1_000, 1_000_000, LATENCY_SEEDS)
            );
        }
        console2.log("  The tx lands faster; the bot still only looks once a second. This row set answers");
        console2.log("  'is a better transaction path worth it on its own'.");

        _latencyHeader("THE WHOLE LOOP AT THAT LATENCY -- polling MOVED WITH the landing latency", LATENCY_SEEDS);
        for (uint256 i; i < latMs.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], latMs[i], 1_000_000, LATENCY_SEEDS)
            );
        }
        console2.log("  This is the flashblocks build: poll `pending` at 200ms AND land in a preconf.");

        _printPickOffMechanism();

        console2.log("");
        console2.log("  READ THIS BEFORE QUOTING A ROW. The two tables agree with each other and with the");
        console2.log("  mechanism: crossing the one-second boundary takes the pool from picked off on every");
        console2.log("  seed to picked off on most of them, worth ~$2,200 of the ~$16,500 a gap costs. And");
        console2.log("  200ms is indistinguishable from 400ms, so the whole of the available gain sits AT");
        console2.log("  the boundary and nothing below it buys anything more. Moving the polling loop as");
        console2.log("  well as the landing adds nothing measurable, because the requote is not what the");
        console2.log("  searcher is racing -- it fires in the same tick the gap prints, before any requote");
        console2.log("  decided on that gap could possibly land, at any latency. What a shorter window");
        console2.log("  removes is the searcher's SECOND through Nth attempt, never its first.");
        console2.log("");
        console2.log("  So a 13% expected saving is the ceiling on what requoting faster is worth, it is");
        console2.log("  bounded by a single draw of the hazard, and two other tests are needed before");
        console2.log("  anyone spends a week on it: the withdrawal race, which is worth more, and the");
        console2.log("  searcher's reaction rate, which decides whether either is worth anything at all.");
    }

    function test_theWithdrawalRaceAcrossTheSecondBoundary() public {
        uint256[4] memory wlat = [uint256(200), 400, 1_000, 2_000];
        uint256 seeds = 6;

        _latencyHeader("JUMP WITHDRAWAL vs a searcher reacting at 1.00/s", seeds);
        _latencyRow("none", _quoteLatCfg(2_000, 1_000, 1_000_000, seeds));
        for (uint256 i; i < wlat.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(wlat[i]), "ms"),
                LatCfg({
                    quoteLatencyMs: 2_000,
                    decisionIntervalMs: 1_000,
                    withdrawLatencyMs: wlat[i],
                    maxPerSecE6: 1_000_000,
                    seeds: seeds
                })
            );
        }

        _latencyHeader("THE SAME, vs a searcher reacting at 5.00/s", seeds);
        _latencyRow("none", _quoteLatCfg(2_000, 1_000, 5_000_000, seeds));
        for (uint256 i; i < wlat.length; ++i) {
            _latencyRow(
                string.concat(FlowModel.u2s(wlat[i]), "ms"),
                LatCfg({
                    quoteLatencyMs: 2_000,
                    decisionIntervalMs: 1_000,
                    withdrawLatencyMs: wlat[i],
                    maxPerSecE6: 5_000_000,
                    seeds: seeds
                })
            );
        }

        console2.log("");
        console2.log("  A single seed of the top block reported the 400ms withdrawal ELIMINATING adverse");
        console2.log("  selection -- toxic share 92.76% -> 0.00%, a $15,600 loss turned into a $66 profit.");
        console2.log("  It is not true, and the shape of the error is the one this file has made before:");
        console2.log("  a number moved a very long way in the desired direction on one sample of a rare");
        console2.log("  binary event. The seeded columns above are what it actually does.");
    }

    uint256 internal constant LATENCY_SEEDS = 8;

    struct LatCfg {
        uint256 quoteLatencyMs;
        uint256 decisionIntervalMs;

        uint256 withdrawLatencyMs;
        uint256 maxPerSecE6;
        uint256 seeds;
    }

    function _latencyHeader(string memory title, uint256 seeds) internal pure {
        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" SWEEP: ", title));
        console2.log(
            string.concat(
                "   300s horizon, +100 bp jump at t=150, $2M epoch, 200ms model tick, ",
                FlowModel.u2s(seeds),
                " seeds a row."
            )
        );
        FlowModel.rule();
        console2.log("  latency      picked off   mean spread($)   mean MO+60s($)   worst seed($)");
    }

    function _latencyRow(string memory label, LatCfg memory c) internal {
        int256 sumSpread;
        int256 sumMo60;
        int256 worst;
        uint256 pickedOff;

        for (uint256 s; s < c.seeds; ++s) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 300;
            p.jumpAtSecs = 150;
            p.tickMs = 200;
            p.quoteLatencyMs = c.quoteLatencyMs;
            p.decisionIntervalMs = c.decisionIntervalMs;
            p.informedMaxPerSecE6 = c.maxPerSecE6;
            if (c.withdrawLatencyMs != 0) {
                p.jumpThresholdE2 = 2_500;
                p.cooloffSecs = 30;
                p.withdrawLatencyMs = c.withdrawLatencyMs;
            }
            p.seed = p.seed + s * 7_919;

            FlowModel.Result memory res = _run(p);
            FlowModel.Accum memory all = _total(res);

            sumSpread += all.spread;
            sumMo60 += all.mo60;
            if (all.spread < worst) worst = all.spread;
            if (res.byPop[FlowModel.POP_INFORMED].fills + res.byPop[FlowModel.POP_FAST].fills != 0) pickedOff += 1;
        }

        int256 n = int256(c.seeds);
        console2.log(
            string.concat(
                "  ",
                FlowModel.pad(label, 13),
                FlowModel.pad(string.concat(FlowModel.u2s(pickedOff), "/", FlowModel.u2s(c.seeds)), 13),
                FlowModel.pad(FlowModel.signedUnits(sumSpread / n, 6), 17),
                FlowModel.pad(FlowModel.signedUnits(sumMo60 / n, 6), 17),
                FlowModel.signedUnits(worst, 6)
            )
        );
    }

    function _quoteLatCfg(uint256 latMs, uint256 decisionMs, uint256 maxPerSecE6, uint256 seeds)
        internal
        pure
        returns (LatCfg memory)
    {
        return LatCfg({
            quoteLatencyMs: latMs,
            decisionIntervalMs: decisionMs,
            withdrawLatencyMs: 0,
            maxPerSecE6: maxPerSecE6,
            seeds: seeds
        });
    }

    function _printPickOffMechanism() internal view {
        FlowModel.Params memory p = _jump();
        p.tickMs = 200;

        uint256 q = FlowModel.informedHazardPerTick(9_400, p);

        console2.log("");
        FlowModel.rule();
        console2.log(" THE MECHANISM, EXACTLY: P(picked off) = 1 - (1 - q)^L on a 94 bp gap");
        console2.log(
            string.concat(
                "   per-200ms-tick hazard q = ",
                FlowModel.pct2(q / 100),
                "%    per-second = ",
                FlowModel.pct2(FlowModel.informedHazard(9_400, p) / 100),
                "%"
            )
        );
        FlowModel.rule();
        console2.log("   latency     window (ticks)   P(the pool is picked off at all)");

        uint256[5] memory latMs = [uint256(200), 400, 1_000, 2_000, 5_000];
        for (uint256 i; i < latMs.length; ++i) {
            uint256 ticks = latMs[i] / 200;
            uint256 survive = FlowModel.PROB_ONE;
            for (uint256 j; j < ticks; ++j) {
                survive = (survive * (FlowModel.PROB_ONE - q)) / FlowModel.PROB_ONE;
            }
            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(string.concat(FlowModel.u2s(latMs[i]), "ms"), 12),
                    FlowModel.pad(FlowModel.u2s(ticks), 17),
                    FlowModel.pct2((FlowModel.PROB_ONE - survive) / 100),
                    "%"
                )
            );
        }
        FlowModel.rule();
    }

    function test_theSubSecondGainIsHostageToTheSearchersReactionRate() public {
        uint256[3] memory rates = [uint256(1_000_000), 2_000_000, 5_000_000];
        uint256[3] memory latMs = [uint256(200), 1_000, 5_000];

        uint256 seeds = 6;
        for (uint256 rr; rr < rates.length; ++rr) {
            _latencyHeader(
                string.concat(
                    "SEARCHER REACTS AT ", FlowModel.pct2(rates[rr] / 10_000), "/s -- whole loop at the latency"
                ),
                seeds
            );
            for (uint256 i; i < latMs.length; ++i) {
                _latencyRow(
                    string.concat(FlowModel.u2s(latMs[i]), "ms"), _quoteLatCfg(latMs[i], latMs[i], rates[rr], seeds)
                );
            }
        }

        console2.log("");
        FlowModel.rule();
        console2.log(" WHAT THE THREE BLOCKS SAY, at 200ms of latency:");
        FlowModel.rule();
        console2.log("   1.00/s  the searcher needs a second to see a 100 bp gap. It gets one draw at ~59%");
        console2.log("           inside the window, plus the latency-advantaged taker's coin flip on the");
        console2.log("           tick the requote lands, so the pool escapes about one run in six.");
        console2.log("   2.00/s  the searcher needs half a second. The per-tick hazard saturates and the");
        console2.log("           escape is already gone: 6/6 picked off, and 200ms is worth $79 against");
        console2.log("           1000ms -- a tenth of one percent of the loss, which is nothing.");
        console2.log("   5.00/s  the searcher reacts within 200ms, the same preconfirmation cadence the");
        console2.log("           maker would be buying. Identical to 2.00/s: the first tick after the gap");
        console2.log("           is taken with certainty and latency stops mattering entirely.");
        console2.log("");
        console2.log("   So the sub-second gain does not survive a searcher twice as fast as the default,");
        console2.log("   let alone one that uses the flashblocks the maker is proposing to buy. Nobody has");
        console2.log("   measured this rate on GIWA. It is not a parameter of the pool, it is a property of");
        console2.log("   the people watching it, and it is the entire case for spending money on speed.");
        FlowModel.rule();
    }

    function test_sensitivityToTheInformedArrivalFunction() public {
        uint256[4] memory e50 = [uint256(250), 500, 1_000, 4_000];

        _sweepHeader("INFORMED ARRIVAL FUNCTION (e50), LARGE gap", "e50", "900s horizon, +100 bp jump at t=450.");
        for (uint256 i; i < e50.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.informedHalfMaxE2 = e50[i];
            FlowModel.Result memory res = _run(p);

            _sweepRow(string.concat(FlowModel.sbps(int256(e50[i])), " bp"), "", res, p);
        }
        console2.log("  Identical. At 100 bp of error the hazard is saturated at every e50 in this range, so");
        console2.log("  the choice changes nothing: somebody takes it, and takes all of it.");

        _sweepHeader("INFORMED ARRIVAL FUNCTION (e50), SMALL gap", "e50", "900s horizon, +12 bp jump at t=450.");
        for (uint256 i; i < e50.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.jumpE2 = 1_200;
            p.informedHalfMaxE2 = e50[i];
            FlowModel.Result memory res = _run(p);

            _sweepRow(string.concat(FlowModel.sbps(int256(e50[i])), " bp"), "", res, p);
        }
        console2.log("  Two things to read here, and only one of them is about e50.");
        console2.log("  (1) e50 moves the answer at errors near the half-spread, and that is the model's");
        console2.log("      weakest joint: whether a marginal gap is taken at all depends on a parameter");
        console2.log("      nobody has measured on this chain. The headline does NOT depend on it -- a");
        console2.log("      100 bp gap is taken under every assumption in the table above.");
        console2.log("  (2) MO+60s comes out POSITIVE here while the spread column is negative. That is");
        console2.log("      not the pool winning; it is one fill of ~$1M sitting inside +-$800 of 60-second");
        console2.log("      diffusion. At small gaps the 60s markout is mostly noise, which is exactly why");
        console2.log("      the per-run report prints +1s and +10s alongside it.");
    }

    function test_sensitivityToTheCaptureAssumption() public {
        uint256[3] memory capture = [uint256(100), 50, 25];
        _sweepHeader("INFORMED CAPTURE (race vs monopolist)", "capture       ");
        for (uint256 i; i < capture.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.informedCapturePct = capture[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(string.concat(FlowModel.u2s(capture[i]), "%"), "", res, p);
        }
    }

    function test_breakEvenUninformedFlow() public {
        uint256[4] memory rates = [uint256(60), 600, 3_600, 10_800];
        _sweepHeader("UNINFORMED FLOW", "arrivals/hr      notional/hr");
        for (uint256 i; i < rates.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.uninformedPerHour = rates[i];
            FlowModel.Result memory res = _run(p);
            _sweepRow(FlowModel.u2s(rates[i]), string.concat("$", FlowModel.thousands(rates[i] * 2_035)), res, p);
        }
        console2.log("  Non-monotone on purpose, and the reason is worth reading: heavy uninformed flow");
        console2.log("  CONSUMES the ask epoch before the gap arrives, so there is less depth left to pick");
        console2.log("  off. Volume is not only the thing that pays for adverse selection, it is also");
        console2.log("  partly what shields the pool from it. Neither effect is large enough to close the");
        console2.log("  gap at any rate this sweep can afford to simulate, so the arithmetic in");
        console2.log("  test_breakEvenArithmetic finishes the job.");
    }

    function test_breakEvenArithmetic() public {
        FlowModel.Params memory jumpRun = _jump();
        jumpRun.horizonSecs = 900;
        jumpRun.jumpAtSecs = 450;
        FlowModel.Result memory jumped = _run(jumpRun);
        int256 toxicLoss = jumped.byPop[FlowModel.POP_INFORMED].mo60 + jumped.byPop[FlowModel.POP_FAST].mo60;

        FlowModel.Result memory quiet = _run(_flat());
        int256 goodBps =
            FlowModel.bpsOf(quiet.byPop[FlowModel.POP_UNINFORMED].mo60, quiet.byPop[FlowModel.POP_UNINFORMED].notional);

        console2.log("");
        console2.log("  BREAK-EVEN, from two measured numbers:");
        console2.log(
            string.concat("    one 100 bp gap against a $2M epoch costs  ", FlowModel.signedUnits(toxicLoss, 6))
        );
        console2.log(string.concat("    uninformed flow earns (flat path, no noise) ", FlowModel.sbps(goodBps), " bp"));
        if (goodBps <= 0 || toxicLoss >= 0) return;

        uint256 needed = (uint256(-toxicLoss) * FlowModel.BPS_E2) / uint256(goodBps) / 1e6;
        console2.log(
            string.concat(
                "    so covering ONE gap needs                $", FlowModel.thousands(needed), " of uninformed notional"
            )
        );
        console2.log(
            string.concat(
                "    at the default 60 arrivals/hour that is   ",
                FlowModel.u2s(needed / (60 * 2_035)),
                " hours of retail flow, per gap"
            )
        );
    }

    function test_sweep_halfSpread() public {
        uint256[4] memory hs = [uint256(5), 15, 30, 60];
        _sweepHeader("HALF-SPREAD (biased: no competing venue in this model)", "half-spread   ");
        for (uint256 i; i < hs.length; ++i) {
            FlowModel.Params memory p = _jump();
            p.horizonSecs = 900;
            p.jumpAtSecs = 450;
            p.halfSpreadBps = hs[i];
            p.widthBps = hs[i] * 5;
            p.informedHalfMaxE2 = hs[i] * 200;
            FlowModel.Result memory res = _run(p);
            _sweepRow(string.concat(FlowModel.u2s(hs[i]), " bp"), "", res, p);
        }
        console2.log("  A wider spread raises the bar a move has to clear before the quote is pickable, so");
        console2.log("  it does help -- but it helps by making the pool refuse to trade, and this model");
        console2.log("  cannot see the volume that refusal costs. Do not read a row here as a recommendation.");
    }

    function test_navReconcilesWithTheUpdatersRiskModule() public {
        FlowModel.Params memory p = _jump();
        p.horizonSecs = 1_800;
        p.jumpAtSecs = 900;
        FlowModel.Result memory res = _run(p);

        int256 fromFills;
        uint256 fills;
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            fromFills += res.byPop[i].spread;
            fills += res.byPop[i].fills;
        }

        console2.log("");
        FlowModel.rule();
        console2.log(" NAV reconciliation against dubu-updater risk.rs");
        FlowModel.rule();
        console2.log(string.concat("  trade PnL from the NAV walk        ", FlowModel.signedUnits(res.tradePnl, 6)));
        console2.log(string.concat("  sum of per-fill realised spread    ", FlowModel.signedUnits(fromFills, 6)));
        console2.log(string.concat("  revaluation (unhedged inventory)   ", FlowModel.signedUnits(res.revaluation, 6)));
        console2.log(
            string.concat("  NAV start ", FlowModel.units(res.navStart, 6), "  end ", FlowModel.units(res.navEnd, 6))
        );
        FlowModel.rule();

        int256 diff = res.tradePnl - fromFills;
        if (diff < 0) diff = -diff;

        assertLe(uint256(diff), 2 * fills + 4, "NAV trade PnL and per-fill markout disagree beyond rounding");

        assertEq(int256(res.navEnd) - int256(res.navStart), res.revaluation + res.tradePnl, "NAV did not decompose");
    }

    function test_informedArrivalFunctionShape() public view {
        FlowModel.Params memory p = _defaults();
        p.tickMs = 200;
        uint256 n = FlowModel.ticksPerSec(p);

        console2.log("");
        FlowModel.rule();
        console2.log(" INFORMED ARRIVAL FUNCTION   lambda(e) = max * n^2/(n^2 + e50^2),  n = max(0, e - cost)");
        console2.log(
            string.concat(
                "   e50 = ",
                FlowModel.sbps(int256(p.informedHalfMaxE2)),
                " bp    cost = ",
                FlowModel.sbps(int256(p.informedCostE2)),
                " bp    max = 1.00/s    tick = 200ms"
            )
        );
        FlowModel.rule();
        console2.log("   edge (bp)   arrivals/sec   P(per 200ms tick)   P(>=1 in one second)   naive lambda/5");

        uint256[9] memory edges = [uint256(0), 50, 100, 200, 500, 1_000, 2_000, 5_000, 20_000];
        for (uint256 i; i < edges.length; ++i) {

            uint256 lambda = FlowModel.informedHazard(int256(edges[i]), p);

            uint256 perTick = FlowModel.informedHazardPerTick(int256(edges[i]), p);

            uint256 survive = FlowModel.PROB_ONE;
            for (uint256 j; j < n; ++j) {
                survive = (survive * (FlowModel.PROB_ONE - perTick)) / FlowModel.PROB_ONE;
            }
            assertApproxEqAbs(
                FlowModel.PROB_ONE - survive, lambda, 20, "the per-tick hazard did not rebuild the per-second one"
            );

            console2.log(
                string.concat(
                    "   ",

                    FlowModel.pad(FlowModel.sbps(int256(edges[i])), 12),
                    FlowModel.pad(FlowModel.pct2(lambda / 10_000), 15),
                    FlowModel.pad(string.concat(FlowModel.pct2(perTick / 100), "%"), 20),
                    FlowModel.pad(string.concat(FlowModel.pct2((FlowModel.PROB_ONE - survive) / 100), "%"), 23),
                    FlowModel.pct2(lambda / n / 100),
                    "%"
                )
            );
        }
        FlowModel.rule();
        console2.log("   The last two columns are the whole point. Column 4 is invariant to the tick by");
        console2.log("   construction; column 5 is what a linear rescaling would have used, and at the");
        console2.log("   saturated top row it is 20.00% per tick -- 67% per second against a true 100%.");
        console2.log("   A 200ms run built on column 5 would show the pool losing less to a jump because");
        console2.log("   the searcher had been quietly disabled, and it would read as a latency finding.");
        FlowModel.rule();

        assertEq(FlowModel.informedHazard(int256(p.informedCostE2), p), 0, "an unprofitable edge attracted flow");
        assertEq(FlowModel.informedHazard(-5_000, p), 0, "a quote in the pool's favour attracted flow");

        uint256 prev;
        for (uint256 e = 100; e <= 20_000; e += 100) {

            uint256 l = FlowModel.informedHazard(int256(e), p);
            assertGe(l, prev, "the hazard is not monotone in the edge");
            assertLe(l, p.informedMaxPerSecE6, "the hazard exceeded its saturation rate");
            prev = l;
        }

        uint256 atHalf = FlowModel.informedHazard(int256(p.informedCostE2 + p.informedHalfMaxE2), p);
        assertEq(atHalf, p.informedMaxPerSecE6 / 2, "e50 is not the half-max point");
    }

    function test_tickRescalingIsResolutionNotExperiment() public view {
        uint256[4] memory ticks = [uint256(1_000), 500, 200, 100];

        console2.log("");
        FlowModel.rule();
        console2.log(" RESCALING INVARIANTS ACROSS tickMs   (sigma 1.00 bp/s, 3600s of path)");
        FlowModel.rule();
        console2.log("   tickMs   ticks/s   realised sigma/s   uninformed fills/hr   informed P(>=1 in 1s) at 100bp");

        for (uint256 i; i < ticks.length; ++i) {
            FlowModel.Params memory p = _defaults();
            p.tickMs = ticks[i];
            p.jumpAtSecs = 0;
            p.driftE2 = 0;

            uint256 sigmaMeasured = _realisedSigmaPerSecE2(p);
            uint256 fillsPerHourE6 = FlowModel.uninformedRatePerTick(p) * FlowModel.ticksPerSec(p) * 3_600;
            uint256 takenInASecond = _informedProbPerSec(p, 10_000);
            uint256 hazardPerSec = FlowModel.informedHazard(10_000, p);

            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(FlowModel.u2s(ticks[i]), 9),
                    FlowModel.pad(FlowModel.u2s(FlowModel.ticksPerSec(p)), 10),

                    FlowModel.pad(string.concat(FlowModel.sbps(int256(sigmaMeasured)), " bp"), 19),
                    FlowModel.pad(FlowModel.pct2(fillsPerHourE6 / 10_000), 22),
                    FlowModel.pct2(takenInASecond / 100),
                    "%"
                )
            );

            assertApproxEqAbs(sigmaMeasured, p.sigmaE2, p.sigmaE2 / 10, "per-second volatility moved with the tick");

            assertApproxEqAbs(
                fillsPerHourE6,
                p.uninformedPerHour * FlowModel.PROB_ONE,
                (p.uninformedPerHour * FlowModel.PROB_ONE) / 1_000,
                "uninformed arrivals per hour moved with the tick"
            );

            assertApproxEqAbs(takenInASecond, hazardPerSec, 20, "P(picked off within a second) moved with the tick");
        }
        FlowModel.rule();
        console2.log("   If the volatility column drifts, every latency conclusion downstream of it is");
        console2.log("   measuring the rescaling. That is why it is measured on a built path rather than");
        console2.log("   asserted from the formula that builds it.");
    }

    function _realisedSigmaPerSecE2(FlowModel.Params memory p) internal pure returns (uint256) {
        uint256[] memory ref = FlowModel.buildPath(p);
        uint256 tps = FlowModel.ticksPerSec(p);
        uint256 n = p.horizonSecs;

        uint256 sumSq;
        for (uint256 s = 1; s <= n; ++s) {
            uint256 a = ref[(s - 1) * tps];
            uint256 b = ref[s * tps];
            uint256 d = b > a ? b - a : a - b;
            uint256 rel = (d * FlowModel.BPS_E2) / a;
            sumSq += rel * rel;
        }

        return FlowModel.isqrt(sumSq / n);
    }

    function _informedProbPerSec(FlowModel.Params memory p, int256 edgeE2) internal pure returns (uint256) {
        uint256 perTick = FlowModel.informedHazardPerTick(edgeE2, p);
        uint256 survive = FlowModel.PROB_ONE;
        for (uint256 j; j < FlowModel.ticksPerSec(p); ++j) {
            survive = (survive * (FlowModel.PROB_ONE - perTick)) / FlowModel.PROB_ONE;
        }
        return FlowModel.PROB_ONE - survive;
    }

    function test_sizeDistributionIsHeavyTailed() public pure {
        FlowModel.Rng memory r = FlowModel.rng(1);
        uint256 n = 20_000;
        uint256[4] memory counts;
        uint256[4] memory notional;
        uint256 total;

        for (uint256 i; i < n; ++i) {
            uint256 v = FlowModel.drawNotional(r);
            uint256 b = FlowModel.bucketOf(v);
            counts[b] += 1;
            notional[b] += v;
            total += v;
        }

        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" UNINFORMED SIZE DISTRIBUTION, ", FlowModel.u2s(n), " draws"));
        FlowModel.rule();
        console2.log("   decade        share of COUNT   share of NOTIONAL");
        for (uint256 b; b < 4; ++b) {
            console2.log(
                string.concat(
                    "   ",
                    FlowModel.pad(FlowModel.bucketLabel(b), 14),
                    FlowModel.pad(string.concat(FlowModel.pct2((counts[b] * 10_000) / n), "%"), 17),
                    FlowModel.pct2((notional[b] * 10_000) / total),
                    "%"
                )
            );
        }
        console2.log(string.concat("   mean notional  $", FlowModel.u2s(total / n)));
        console2.log("   A Pareto tail index of 1: each decade is a tenth as likely and carries the same");
        console2.log("   expected notional. Most flow is small; the top 0.1% of orders is ~a quarter of it.");
        FlowModel.rule();

        assertApproxEqAbs(counts[0], (n * 900) / 1_000, n / 100, "decade 0 weight");
        assertApproxEqAbs(counts[1], (n * 90) / 1_000, n / 100, "decade 1 weight");
        assertGt(counts[2], 0, "the $10k-100k decade never fired");

        assertGt(notional[2] + notional[3], total / 3, "the tail carries too little of the notional");
    }

    function test_aQuietBookAttributesExactlyZeroTradePnl() public {
        FlowModel.Params memory p = _defaults();
        p.horizonSecs = 300;
        p.uninformedPerHour = 0;
        p.informedMaxPerSecE6 = 0;
        p.fastHitPct = 0;
        p.jumpAtSecs = 150;
        p.jumpE2 = 20_000;

        FlowModel.Result memory res = _run(p);

        assertEq(res.byPop[FlowModel.POP_UNINFORMED].fills, 0, "the quiet run traded");
        assertEq(res.tradePnl, 0, "a book with no fills attributed non-zero trade PnL");
        assertTrue(res.revaluation != 0, "a 200 bp move produced no revaluation");

        assertEq(int256(res.navEnd) - int256(res.navStart), res.revaluation, "NAV moved for a reason other than price");

        console2.log("");
        console2.log(
            string.concat(
                "  Quiet book, 200 bp gap: revaluation ",
                FlowModel.signedUnits(res.revaluation, 6),
                ", trade PnL ",
                FlowModel.signedUnits(res.tradePnl, 6),
                " (exactly zero, as risk.rs requires)"
            )
        );
    }

    function test_informedSizingStopsExactlyWhereTheProfitStops() public {
        FlowModel.Params memory p = _defaults();
        _deployFixture(p);
        _pushLadder(p, REF_MID);
        _refresh(p);

        uint256 ref = (REF_MID * 10_100) / 10_000;
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        uint256 refNet = FlowModel.refNetForAsk(ref, p.informedCostE2);

        uint256 q = FlowModel.informedAskSize(s, s.askCapacity, refNet, SCALE, pool.reserveOf(address(baseToken)), 100);
        assertGt(q, 0, "a 100 bp stale ask attracted no size");

        uint256 cost = PropCurve.amountInAsk(q, s.minAsk, s.maxAsk, s.askCapacity, 0, PRICE_SCALE_EXP);
        assertLe(cost * SCALE, q * refNet, "the solver returned a size the taker would not take");
        if (q < s.askCapacity) {
            uint256 costNext = PropCurve.amountInAsk(q + 1, s.minAsk, s.maxAsk, s.askCapacity, 0, PRICE_SCALE_EXP);
            assertGt(costNext * SCALE, (q + 1) * refNet, "the solver left profitable depth on the table");
        }

        console2.log("");
        console2.log(
            string.concat(
                "  100 bp stale ask, $2M epoch: the informed taker lifts ",
                FlowModel.units(q, 18),
                " base of the ",
                FlowModel.units(s.askCapacity, 18),
                " on offer."
            )
        );
        console2.log("  It clears the WHOLE epoch, because 100 bp of error dwarfs a 25 bp ladder width.");
        console2.log("  The ladder's shape stops protecting anything once the error exceeds its width.");
    }

    function _run(FlowModel.Params memory p) internal returns (FlowModel.Result memory res) {

        vm.pauseGasMetering();

        uint256[] memory ref = FlowModel.buildPath(p);
        _deployFixture(p);

        vm.warp(T0);
        _pushLadder(p, ref[0]);
        _refresh(p);
        delete S;
        S.lastPushAt = 0;
        S.ticksPerSec = FlowModel.ticksPerSec(p);
        S.quoteLatencyTicks = FlowModel.msToTicks(p.quoteLatencyMs, p);
        S.withdrawLatencyTicks = FlowModel.msToTicks(p.withdrawLatencyMs, p);
        S.cooloffTicks = FlowModel.secsToTicks(p.cooloffSecs, p);
        S.heartbeatTicks = FlowModel.secsToTicks(p.heartbeatSecs, p);
        S.decisionTicks = FlowModel.msToTicks(p.decisionIntervalMs, p);
        S.detectorTicks = FlowModel.msToTicks(p.detectorIntervalMs, p);

        FlowModel.Rng memory r = FlowModel.rng(p.seed);

        res.baseStart = pool.reserveOf(address(baseToken));
        res.quoteStart = pool.reserveOf(address(quoteToken));
        res.baseMin = res.baseStart;
        res.baseMax = res.baseStart;
        res.navStart = res.quoteStart + FlowModel.value(res.baseStart, ref[0], p.priceScaleExp);

        _observe(res, ref[0], p, false);

        uint256 arena;

        assembly ("memory-safe") {
            arena := mload(0x40)
        }

        uint256 nTicks = FlowModel.totalTicks(p);
        uint256 prevSecs;
        for (uint256 k = 1; k <= nTicks; ++k) {

            assembly {
                mstore(0x40, arena)
            }

            uint256 secs = FlowModel.tickToBlockSecs(k, p);
            if (secs != prevSecs) {
                res.blockSecsAdvanced += 1;
                prevSecs = secs;
            }
            vm.warp(T0 + secs);

            _observe(res, ref[k], p, true);

            _jumpTick(p, ref, k);

            _fastTaker(res, p, r, ref, k);
            _landWithdrawal(p, k);
            _landPending(res, p, k);
            _informed(res, p, r, ref, k);
            _uninformed(res, p, r, ref, k);
            _observe(res, ref[k], p, false);
            _updaterTick(p, ref, k);

            uint256 b = pool.reserveOf(address(baseToken));
            if (b < res.baseMin) res.baseMin = b;
            if (b > res.baseMax) res.baseMax = b;
        }

        res.ticksRun = nTicks;
        res.baseEnd = pool.reserveOf(address(baseToken));
        res.quoteEnd = pool.reserveOf(address(quoteToken));
        res.navEnd = res.quoteEnd + FlowModel.value(res.baseEnd, ref[nTicks], p.priceScaleExp);

        vm.resumeGasMetering();
    }

    function _jumpTick(FlowModel.Params memory p, uint256[] memory ref, uint256 k) internal {
        if (p.jumpThresholdE2 == 0) return;
        if (k % S.detectorTicks != 0) return;
        if (S.withdrawLandsAt != 0 || k < S.cooloffUntil) return;

        uint256 back = k > S.ticksPerSec ? k - S.ticksPerSec : 0;
        if (!FlowModel.isJump(ref[back], ref[k], p.jumpThresholdE2)) return;

        S.jumpsDetected += 1;
        S.withdrawLandsAt = k + S.withdrawLatencyTicks;
    }

    function _landWithdrawal(FlowModel.Params memory, uint256 k) internal {
        if (S.withdrawLandsAt == 0 || k < S.withdrawLandsAt) return;

        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, 0, 0);

        S.withdrawLandsAt = 0;
        S.cooloffUntil = k + S.cooloffTicks;

        S.pendingLandsAt = 0;
        S.pendingRefresh = false;
    }

    function _updaterTick(FlowModel.Params memory p, uint256[] memory ref, uint256 k) internal {

        if (k % S.decisionTicks != 0) return;
        if (S.withdrawLandsAt != 0 || k < S.cooloffUntil) return;
        if (S.pendingLandsAt != 0) return;

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        bool capacityLow = _capacityIsLow(s, p);

        if (!capacityLow && (k - S.lastPushAt) < S.heartbeatTicks && !_driftTriggers(s, p, ref[k])) return;

        S.pendingMid = ref[k];
        S.pendingLandsAt = k + S.quoteLatencyTicks;
        S.pendingRefresh = capacityLow;
    }

    function _capacityIsLow(IPropPool.PairSnapshot memory s, FlowModel.Params memory p) internal pure returns (bool) {
        (uint256 cap, uint256 used) = FlowModel.room(s, true);
        if (cap == 0) return true;
        if ((cap - used) * 100 < cap * (100 - p.capacityDivergencePct)) return true;
        (cap, used) = FlowModel.room(s, false);
        if (cap == 0) return true;
        return (cap - used) * 100 < cap * (100 - p.capacityDivergencePct);
    }

    function _driftTriggers(IPropPool.PairSnapshot memory s, FlowModel.Params memory p, uint256 ref)
        internal
        pure
        returns (bool)
    {
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref);

        int256 hs = int256(FlowModel.halfSpreadAt(p) * 100);

        if (bidEdge != type(int256).min && _fires(bidEdge + hs, p)) return true;
        if (askEdge != type(int256).min && _fires(askEdge + hs, p)) return true;
        return false;
    }

    function _fires(int256 devE2, FlowModel.Params memory p) private pure returns (bool) {

        if (devE2 >= int256(p.adverseDriftE2)) return true;

        return -devE2 >= int256(p.favourableDriftE2);
    }

    function _landPending(FlowModel.Result memory res, FlowModel.Params memory p, uint256 t) internal {
        if (S.pendingLandsAt == 0 || t < S.pendingLandsAt) return;

        _pushLadder(p, S.pendingMid);
        res.quotePushes += 1;
        if (S.pendingRefresh) {
            _refresh(p);
            res.capacityRefreshes += 1;
        }
        S.pendingLandsAt = 0;
        S.pendingRefresh = false;
        S.lastPushAt = t;
    }

    function _fastTaker(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        if (p.fastHitPct == 0) return;

        if (S.pendingLandsAt != k) return;
        if (!FlowModel.bernoulli(r, p.fastHitPct * 10_000)) return;
        _takeTheEdge(res, p, ref, k, FlowModel.POP_FAST);
    }

    function _informed(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        if (p.informedMaxPerSecE6 == 0) return;

        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref[k]);
        int256 best = bidEdge > askEdge ? bidEdge : askEdge;
        if (!FlowModel.bernoulli(r, FlowModel.informedHazardPerTick(best, p))) return;

        _takeTheEdge(res, p, ref, k, FlowModel.POP_INFORMED);
    }

    function _takeTheEdge(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop
    ) internal {
        IPropPool.PairSnapshot memory s = pool.snapshot(PAIR_ID);
        (int256 bidEdge, int256 askEdge) = FlowModel.edges(s, ref[k]);

        if (bidEdge <= int256(p.informedCostE2) && askEdge <= int256(p.informedCostE2)) return;

        if (askEdge >= bidEdge) _liftTheAsk(res, p, ref, k, pop, s);
        else _hitTheBid(res, p, ref, k, pop, s);
    }

    function _liftTheAsk(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop,
        IPropPool.PairSnapshot memory s
    ) internal {
        uint256 q = FlowModel.informedAskSize(
            s,
            FlowModel.fillableRoom(pool, PAIR_ID, s, false),
            FlowModel.refNetForAsk(ref[k], p.informedCostE2),
            SCALE,
            pool.reserveOf(address(baseToken)),
            p.informedCapturePct
        );
        if (q == 0) return;
        uint256 cost = pool.getAmountIn(address(quoteToken), address(baseToken), q);
        if (cost == 0) return;

        _mintTo(taker, address(quoteToken), cost);
        vm.prank(taker);

        pool.swap(address(quoteToken), address(baseToken), -int256(q), type(uint256).max, taker, 0, block.timestamp);

        _record(res, FlowModel.Fill({pop: pop, tTick: k, isBid: false, baseQty: q, quoteQty: cost}), ref, p);
    }

    function _hitTheBid(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 pop,
        IPropPool.PairSnapshot memory s
    ) internal {
        uint256 q = FlowModel.informedBidSize(
            s,
            FlowModel.fillableRoom(pool, PAIR_ID, s, true),
            FlowModel.refNetForBid(ref[k], p.informedCostE2),
            SCALE,
            pool.reserveOf(address(quoteToken)),
            p.informedCapturePct
        );
        if (q == 0) return;
        uint256 proceeds = pool.getAmountOut(address(baseToken), address(quoteToken), q);
        if (proceeds == 0) return;

        _mintTo(taker, address(baseToken), q);
        vm.prank(taker);

        pool.swap(address(baseToken), address(quoteToken), int256(q), 0, taker, 0, block.timestamp);

        _record(res, FlowModel.Fill({pop: pop, tTick: k, isBid: true, baseQty: q, quoteQty: proceeds}), ref, p);
    }

    function _record(
        FlowModel.Result memory res,
        FlowModel.Fill memory f,
        uint256[] memory ref,
        FlowModel.Params memory p
    ) internal {
        if (S.withdrawLandsAt != 0) S.fillsWhileWithdrawing += 1;
        FlowModel.record(res, f, ref, p);
    }

    function _uninformed(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        FlowModel.Rng memory r,
        uint256[] memory ref,
        uint256 k
    ) internal {
        uint256 rate = FlowModel.uninformedRatePerTick(p);
        uint256 n = rate / FlowModel.PROB_ONE;
        if (FlowModel.bernoulli(r, rate % FlowModel.PROB_ONE)) n += 1;

        for (uint256 i; i < n; ++i) {
            uint256 raw = FlowModel.drawNotional(r) * (10 ** uint256(p.quoteDecimals));
            if (FlowModel.next(r) % 2 == 0) _uninformedBuysBase(res, p, ref, k, raw);
            else _uninformedSellsBase(res, p, ref, k, raw);
        }
    }

    function _uninformedBuysBase(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 raw
    ) internal {
        uint256 out = pool.getAmountOut(address(quoteToken), address(baseToken), raw);
        if (out == 0) {
            res.declinedFills += 1;
            res.declinedNotional += raw;
            return;
        }
        _mintTo(taker, address(quoteToken), raw);
        vm.prank(taker);

        pool.swap(address(quoteToken), address(baseToken), int256(raw), 0, taker, 0, block.timestamp);
        _record(
            res,
            FlowModel.Fill({pop: FlowModel.POP_UNINFORMED, tTick: k, isBid: false, baseQty: out, quoteQty: raw}),
            ref,
            p
        );
    }

    function _uninformedSellsBase(
        FlowModel.Result memory res,
        FlowModel.Params memory p,
        uint256[] memory ref,
        uint256 k,
        uint256 raw
    ) internal {
        uint256 baseIn = (raw * SCALE) / ref[k];
        uint256 out = pool.getAmountOut(address(baseToken), address(quoteToken), baseIn);
        if (out == 0) {
            res.declinedFills += 1;
            res.declinedNotional += raw;
            return;
        }
        _mintTo(taker, address(baseToken), baseIn);
        vm.prank(taker);

        pool.swap(address(baseToken), address(quoteToken), int256(baseIn), 0, taker, 0, block.timestamp);
        _record(
            res,
            FlowModel.Fill({pop: FlowModel.POP_UNINFORMED, tTick: k, isBid: true, baseQty: baseIn, quoteQty: out}),
            ref,
            p
        );
    }

    function _observe(FlowModel.Result memory res, uint256 fair, FlowModel.Params memory p, bool expectNoTrade)
        internal
    {
        uint256 q = pool.reserveOf(address(quoteToken));
        uint256 b = pool.reserveOf(address(baseToken));

        if (!S.navSeeded) {
            S.prevQuote = q;
            S.prevBase = b;
            S.prevFair = fair;
            S.navSeeded = true;
            return;
        }

        int256 reval = int256(FlowModel.value(S.prevBase, fair, p.priceScaleExp))

            - int256(FlowModel.value(S.prevBase, S.prevFair, p.priceScaleExp));

        int256 prevNav = int256(S.prevQuote + FlowModel.value(S.prevBase, S.prevFair, p.priceScaleExp));

        int256 nav = int256(q + FlowModel.value(b, fair, p.priceScaleExp));

        int256 tradePnl = (nav - prevNav) - reval;
        if (expectNoTrade) assertEq(tradePnl, 0, "a market move with no fill attributed trade PnL");

        res.revaluation += reval;
        res.tradePnl += tradePnl;

        S.prevQuote = q;
        S.prevBase = b;
        S.prevFair = fair;
    }

    function _deployFixture(FlowModel.Params memory p) internal {
        baseToken = new MockERC20("Base", "BASE", p.baseDecimals);
        quoteToken = new MockERC20("Quote", "QUOTE", p.quoteDecimals);
        pool = new PropPool(owner, manager, updater, guardian);

        vm.prank(owner);

        pool.addPair(address(baseToken), address(quoteToken), p.priceScaleExp, MAX_STALE_SECS, uint56(p.mid / 10));

        if (p.decaySecs != 0) {
            vm.prank(manager);
            pool.setPairDecay(PAIR_ID, p.decaySecs);
        }

        baseToken.mint(manager, TVL_BASE);
        quoteToken.mint(manager, TVL_QUOTE);
        vm.startPrank(manager);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        pool.deposit(address(baseToken), TVL_BASE);
        pool.deposit(address(quoteToken), TVL_QUOTE);
        vm.stopPrank();

        vm.startPrank(taker);
        baseToken.approve(address(pool), type(uint256).max);
        quoteToken.approve(address(pool), type(uint256).max);
        vm.stopPrank();
    }

    function _pushLadder(FlowModel.Params memory p, uint256 mid) internal {
        (,,, uint256 maxAsk) = FlowModel.ladder(mid, FlowModel.halfSpreadAt(p), p.widthBps);
        require(maxAsk <= type(uint56).max, "Simulation: the path took the ladder outside uint56");

        uint256[] memory packed = new uint256[](1);
        packed[0] = FlowModel.packLadder(PAIR_ID, mid, FlowModel.halfSpreadAt(p), p.widthBps);
        vm.prank(updater);
        pool.updateQuote(packed);
    }

    function _refresh(FlowModel.Params memory p) internal {
        vm.prank(updater);
        pool.refreshCapacity(PAIR_ID, p.bidCapacity, p.askCapacity);
    }

    function _mintTo(address who, address token, uint256 amount) internal {
        MockERC20(token).mint(who, amount);
    }

    function _report(FlowModel.Result memory res, FlowModel.Params memory p) internal pure {
        FlowModel.printAttribution(res, p.quoteDecimals);
        FlowModel.printBuckets(res, p.quoteDecimals);
        FlowModel.printNav(res, p.baseDecimals, p.quoteDecimals);
        if (res.worstFillNotional != 0) {
            console2.log(
                string.concat(
                    "    worst single fill: ",
                    FlowModel.signedUnits(res.worstFillMo60, p.quoteDecimals),
                    " at +60s on ",
                    FlowModel.units(res.worstFillNotional, p.quoteDecimals),
                    " of notional, at t=",
                    FlowModel.u2s(FlowModel.tickToMs(res.worstFillAtTick, p)),
                    "ms"
                )
            );
        }
        FlowModel.printVerdict(res, p.quoteDecimals);
    }

    function _sweepHeader(string memory title, string memory firstCols) internal pure {
        _sweepHeader(title, firstCols, "900s horizon, +100 bp jump at t=450, everything else at the defaults.");
    }

    function _sweepHeader(string memory title, string memory firstCols, string memory setup) internal pure {
        console2.log("");
        FlowModel.rule();
        console2.log(string.concat(" SWEEP: ", title));
        console2.log(string.concat("   ", setup));
        FlowModel.rule();
        console2.log(
            string.concat("  ", FlowModel.pad(firstCols, 30), "fills  toxic%   spread($)      MO+60s($)     MO+60s(bp)")
        );
    }

    function _sweepRow(string memory col1, string memory col2, FlowModel.Result memory res, FlowModel.Params memory p)
        internal
        pure
    {
        FlowModel.Accum memory all = _total(res);
        uint256 toxic = res.byPop[FlowModel.POP_INFORMED].notional + res.byPop[FlowModel.POP_FAST].notional;

        string memory left = string.concat(
            "  ",
            FlowModel.pad(col1, 18),
            FlowModel.pad(col2, 12),
            FlowModel.pad(FlowModel.u2s(all.fills), 7),
            FlowModel.pad(FlowModel.pct2(all.notional == 0 ? 0 : (toxic * 10_000) / all.notional), 9)
        );
        console2.log(
            string.concat(
                left,
                FlowModel.pad(FlowModel.signedUnits(all.spread, p.quoteDecimals), 15),
                FlowModel.pad(FlowModel.signedUnits(all.mo60, p.quoteDecimals), 14),
                FlowModel.sbps(FlowModel.bpsOf(all.mo60, all.notional))
            )
        );
    }

    function _total(FlowModel.Result memory res) internal pure returns (FlowModel.Accum memory all) {
        for (uint256 i; i < FlowModel.POP_COUNT; ++i) {
            all.fills += res.byPop[i].fills;
            all.notional += res.byPop[i].notional;
            all.spread += res.byPop[i].spread;
            all.mo1 += res.byPop[i].mo1;
            all.mo10 += res.byPop[i].mo10;
            all.mo60 += res.byPop[i].mo60;
        }
    }
}
