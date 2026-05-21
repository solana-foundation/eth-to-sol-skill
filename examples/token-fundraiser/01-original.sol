// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @title Crowdfund
/// @notice One-shot ERC-20 crowdfund. The creator declares a goal and a
///         deadline; supporters call `contribute()` to commit tokens. If the
///         goal is reached before the deadline, the creator can `claim()`
///         the entire pot. If it isn't, supporters can `refund()` whatever
///         they put in.
///
/// Reference: the Solana version is the `tokens/token-fundraiser` example
/// from solana-developers/program-examples — same lifecycle, different
/// state model.
contract Crowdfund {
    IERC20 public immutable token;
    address public immutable creator;
    uint256 public immutable goal;
    uint256 public immutable deadline;

    uint256 public totalRaised;
    bool public claimed;

    /// Per-supporter ledger so refunds know who put in what.
    mapping(address => uint256) public contributions;

    error Ended();
    error NotEnded();
    error NotCreator();
    error GoalMet();
    error GoalNotMet();
    error AlreadyClaimed();
    error NothingToRefund();
    error ZeroAmount();

    event Contributed(address indexed supporter, uint256 amount);
    event Claimed(address indexed creator, uint256 amount);
    event Refunded(address indexed supporter, uint256 amount);

    constructor(
        IERC20 _token,
        address _creator,
        uint256 _goal,
        uint256 _duration
    ) {
        token = _token;
        creator = _creator;
        goal = _goal;
        deadline = block.timestamp + _duration;
    }

    /// Supporter pulls tokens into the contract and ledger is updated.
    function contribute(uint256 amount) external {
        if (amount == 0) revert ZeroAmount();
        if (block.timestamp >= deadline) revert Ended();

        token.transferFrom(msg.sender, address(this), amount);
        contributions[msg.sender] += amount;
        totalRaised += amount;

        emit Contributed(msg.sender, amount);
    }

    /// Creator sweeps the pot once the goal is met. Single-shot.
    function claim() external {
        if (msg.sender != creator) revert NotCreator();
        if (claimed) revert AlreadyClaimed();
        if (totalRaised < goal) revert GoalNotMet();

        claimed = true;
        uint256 amount = totalRaised;
        token.transfer(creator, amount);

        emit Claimed(creator, amount);
    }

    /// Supporter pulls their tokens back if the goal wasn't met by the
    /// deadline.
    function refund() external {
        if (block.timestamp < deadline) revert NotEnded();
        if (totalRaised >= goal) revert GoalMet();

        uint256 amount = contributions[msg.sender];
        if (amount == 0) revert NothingToRefund();

        contributions[msg.sender] = 0;
        token.transfer(msg.sender, amount);

        emit Refunded(msg.sender, amount);
    }
}
