// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/// @title StakingRewards
/// @notice Single-asset staking with continuous reward emission.
/// @dev Synthetix-style accumulator. Users stake `stakingToken`, accrue `rewardsToken`
///      at `rewardRate` tokens per second, distributed pro-rata across all stakers.
///      Owner sets the rate and funds the rewards pool externally (out of scope here).
contract StakingRewards {
    // ---- Immutable config ----
    IERC20 public immutable stakingToken;
    IERC20 public immutable rewardsToken;

    // ---- Ownership ----
    address public owner;

    // ---- Reward stream ----
    uint256 public rewardRate;            // tokens per second, distributed across all stakers
    uint256 public lastUpdateTime;        // unix seconds when accumulator was last refreshed
    uint256 public rewardPerTokenStored;  // accumulator: cumulative reward per staked token (scaled 1e18)

    // ---- Aggregates ----
    uint256 public totalSupply;

    // ---- Per-user ----
    mapping(address => uint256) public balanceOf;
    mapping(address => uint256) public userRewardPerTokenPaid;  // checkpoint at last interaction
    mapping(address => uint256) public rewards;                  // accrued, unclaimed

    // ---- Events ----
    event Staked(address indexed user, uint256 amount);
    event Withdrawn(address indexed user, uint256 amount);
    event RewardPaid(address indexed user, uint256 reward);
    event RewardRateUpdated(uint256 oldRate, uint256 newRate);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    // ---- Errors ----
    error NotOwner();
    error ZeroAmount();
    error InsufficientBalance();
    error ZeroAddress();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    /// @notice Refresh global accumulator + the caller's checkpointed reward.
    /// @dev The Synthetix trick: lazily extrapolate `rewardPerToken` from
    ///      `rewardPerTokenStored` using `lastUpdateTime` and the current rate.
    ///      Eagerly checkpoint on every state-changing interaction.
    modifier updateReward(address account) {
        rewardPerTokenStored = rewardPerToken();
        lastUpdateTime = block.timestamp;
        if (account != address(0)) {
            rewards[account] = earned(account);
            userRewardPerTokenPaid[account] = rewardPerTokenStored;
        }
        _;
    }

    constructor(address _stakingToken, address _rewardsToken) {
        if (_stakingToken == address(0) || _rewardsToken == address(0)) revert ZeroAddress();
        stakingToken = IERC20(_stakingToken);
        rewardsToken = IERC20(_rewardsToken);
        owner = msg.sender;
        emit OwnershipTransferred(address(0), msg.sender);
    }

    // ---- Views ----

    function rewardPerToken() public view returns (uint256) {
        if (totalSupply == 0) return rewardPerTokenStored;
        uint256 dt = block.timestamp - lastUpdateTime;
        return rewardPerTokenStored + (dt * rewardRate * 1e18) / totalSupply;
    }

    function earned(address account) public view returns (uint256) {
        return (balanceOf[account] * (rewardPerToken() - userRewardPerTokenPaid[account])) / 1e18
            + rewards[account];
    }

    // ---- User actions ----

    function stake(uint256 amount) external updateReward(msg.sender) {
        if (amount == 0) revert ZeroAmount();
        totalSupply += amount;
        balanceOf[msg.sender] += amount;
        stakingToken.transferFrom(msg.sender, address(this), amount);
        emit Staked(msg.sender, amount);
    }

    function withdraw(uint256 amount) public updateReward(msg.sender) {
        if (amount == 0) revert ZeroAmount();
        if (balanceOf[msg.sender] < amount) revert InsufficientBalance();
        totalSupply -= amount;
        balanceOf[msg.sender] -= amount;
        stakingToken.transfer(msg.sender, amount);
        emit Withdrawn(msg.sender, amount);
    }

    function getReward() public updateReward(msg.sender) {
        uint256 reward = rewards[msg.sender];
        if (reward > 0) {
            rewards[msg.sender] = 0;
            rewardsToken.transfer(msg.sender, reward);
            emit RewardPaid(msg.sender, reward);
        }
    }

    function exit() external {
        withdraw(balanceOf[msg.sender]);
        getReward();
    }

    // ---- Admin ----

    function setRewardRate(uint256 newRate) external onlyOwner updateReward(address(0)) {
        uint256 old = rewardRate;
        rewardRate = newRate;
        emit RewardRateUpdated(old, newRate);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        address prev = owner;
        owner = newOwner;
        emit OwnershipTransferred(prev, newOwner);
    }
}
