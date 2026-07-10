// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {MockERC20} from "./MockERC20.sol";

/// @title MintableToken
/// @notice `MockERC20` plus a public, rate-limited faucet.
///
/// `MockERC20.mint` already lets anyone create supply, but a visitor who arrives at the demo
/// without tokens should not have to be told to hand-craft a `mint(address,uint256)` call in a
/// block explorer. `claim()` is the one-button version of that: a fixed drip, once per address
/// per 24 hours, no allowlist, no operator in the loop.
///
/// The cooldown is not a security control — nothing here is scarce, and anyone determined to
/// hold a billion of these can call `mint` directly. It exists so a single visitor tapping the
/// button repeatedly does not turn the demo dashboard's supply chart into a vertical line, and
/// so "how much does one visitor get" is a fixed, quotable number.
///
/// @dev ⚠️ TESTNET PROP — NEVER DEPLOY TO MAINNET. ⚠️ Inherits every warning on `MockERC20`,
///      and adds an unauthenticated mint path on top.
contract MintableToken is MockERC20 {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    /// @param availableAt unix timestamp at which `recipient` may claim again
    error ClaimTooSoon(uint256 availableAt);
    error InvalidRecipient();
    error ZeroClaimAmount();

    // ---------------------------------------------------------------------
    // Events
    // ---------------------------------------------------------------------

    /// @param recipient who received the drip and whose cooldown was consumed
    /// @param caller    who paid the gas — differs from `recipient` on a sponsored claim
    event Claimed(address indexed recipient, address indexed caller, uint256 amount);

    // ---------------------------------------------------------------------
    // Faucet state
    // ---------------------------------------------------------------------

    /// @notice Minimum spacing between two claims for the same recipient.
    uint256 public constant CLAIM_INTERVAL = 24 hours;

    /// @notice Fixed drip size, in this token's smallest unit (so scale it by `10 ** decimals`
    ///         at deployment — a 6-decimal token wanting 10,000 units passes `10_000e6`).
    /// @dev Immutable, and there is no setter. A mutable drip would need an owner, an owner
    ///      would need a transfer path, and none of that buys anything for a token whose supply
    ///      is already unrestricted. Redeploy if the number is wrong.
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    uint256 public immutable claimAmount;

    /// @notice Timestamp of each address's last successful claim. Zero means "never claimed".
    mapping(address => uint256) public lastClaimAt;

    // ---------------------------------------------------------------------
    // Construction
    // ---------------------------------------------------------------------

    constructor(string memory name_, string memory symbol_, uint8 decimals_, uint256 claimAmount_)
        MockERC20(name_, symbol_, decimals_)
    {
        // A zero drip would let `claim` succeed while doing nothing, which reads as a working
        // faucet in the UI and an empty wallet everywhere else.
        if (claimAmount_ == 0) revert ZeroClaimAmount();
        claimAmount = claimAmount_;
    }

    // ---------------------------------------------------------------------
    // Faucet
    // ---------------------------------------------------------------------

    /// @notice Mint the fixed drip to the caller.
    /// @return amount the amount minted, so a contract caller does not have to read `claimAmount`
    function claim() external returns (uint256 amount) {
        return _claim(msg.sender);
    }

    /// @notice Mint the fixed drip to `recipient`, paid for by the caller.
    /// @dev The demo frontend uses this for a visitor whose wallet has no GIWA ETH at all —
    ///      faucet ETH is capped at 0.015/day and often exhausted, so "connect a fresh wallet
    ///      and trade" has to work with a zero balance.
    ///
    ///      The cooldown is keyed on `recipient`, not on `msg.sender`. It has to be: a sponsor
    ///      is a single relayer address serving every visitor, and keying on the payer would cap
    ///      the entire demo at one claim per day. The cost of that choice is that anyone can
    ///      call `claimFor(victim)` to reset a third party's timer and make their own `claim()`
    ///      revert for the next 24 hours. The victim still receives the tokens, so the worst
    ///      case is unsolicited free money at an inconvenient moment — an acceptable trade for a
    ///      testnet faucet, and not fixable without either an operator signature or per-caller
    ///      accounting that reintroduces the relayer cap.
    function claimFor(address recipient) external returns (uint256 amount) {
        return _claim(recipient);
    }

    /// @notice Timestamp at which `account` may next claim. Zero if it has never claimed.
    function nextClaimAt(address account) public view returns (uint256) {
        uint256 last = lastClaimAt[account];
        return last == 0 ? 0 : last + CLAIM_INTERVAL;
    }

    /// @notice Whether a claim for `account` would succeed right now. For greying out the button.
    function canClaim(address account) external view returns (bool) {
        return block.timestamp >= nextClaimAt(account);
    }

    function _claim(address recipient) internal returns (uint256 amount) {
        if (recipient == address(0)) revert InvalidRecipient();

        uint256 last = lastClaimAt[recipient];
        // `last == 0` means "never claimed", not "claimed at the unix epoch", and the two must
        // be distinguished explicitly. Foundry starts `block.timestamp` at 1, so on a fresh test
        // chain `0 + CLAIM_INTERVAL` sits in the future and a naive comparison would reject
        // every first-ever claim.
        if (last != 0) {
            uint256 availableAt = last + CLAIM_INTERVAL;
            if (block.timestamp < availableAt) revert ClaimTooSoon(availableAt);
        }

        // Written before the mint. `_mint` cannot re-enter — there is no callback anywhere in
        // MockERC20 — but the ordering costs nothing and stops that from becoming load-bearing
        // if someone later subclasses this with a hook.
        lastClaimAt[recipient] = block.timestamp;

        amount = claimAmount;
        _mint(recipient, amount);
        emit Claimed(recipient, msg.sender, amount);
    }
}
