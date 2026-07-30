// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {MockERC20} from "./MockERC20.sol";

contract MintableToken is MockERC20 {

    error ClaimTooSoon(uint256 availableAt);
    error InvalidRecipient();
    error ZeroClaimAmount();

    event Claimed(address indexed recipient, address indexed caller, uint256 amount);

    uint256 public constant CLAIM_INTERVAL = 24 hours;

    uint256 public immutable claimAmount;

    mapping(address => uint256) public lastClaimAt;

    constructor(string memory name_, string memory symbol_, uint8 decimals_, uint256 claimAmount_)
        MockERC20(name_, symbol_, decimals_)
    {

        if (claimAmount_ == 0) revert ZeroClaimAmount();
        claimAmount = claimAmount_;
    }

    function claim() external returns (uint256 amount) {
        return _claim(msg.sender);
    }

    function claimFor(address recipient) external returns (uint256 amount) {
        return _claim(recipient);
    }

    function nextClaimAt(address account) public view returns (uint256) {
        uint256 last = lastClaimAt[account];
        return last == 0 ? 0 : last + CLAIM_INTERVAL;
    }

    function canClaim(address account) external view returns (bool) {
        return block.timestamp >= nextClaimAt(account);
    }

    function _claim(address recipient) internal returns (uint256 amount) {
        if (recipient == address(0)) revert InvalidRecipient();

        uint256 last = lastClaimAt[recipient];

        if (last != 0) {
            uint256 availableAt = last + CLAIM_INTERVAL;
            if (block.timestamp < availableAt) revert ClaimTooSoon(availableAt);
        }

        lastClaimAt[recipient] = block.timestamp;

        amount = claimAmount;
        _mint(recipient, amount);
        emit Claimed(recipient, msg.sender, amount);
    }
}
