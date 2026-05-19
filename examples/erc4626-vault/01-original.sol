// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
    function decimals() external view returns (uint8);
}

/// @title ExampleVault — minimal ERC-4626 with virtual-offset inflation defense and yield fee.
/// @notice Shares are themselves ERC-20-shaped (transfer/approve on the share token). This
///         contract focuses on the 4626-specific deposit/mint/withdraw/redeem/earn surface
///         and inherits the ERC-20 share-token plumbing from a base class. The naive Anchor
///         port omits the share-transfer/approve surface for brevity (already exercised by
///         the ERC-20 reference example); the optimized port reintroduces them via SPL Token.
/// @dev OpenZeppelin-style: virtualShares = 10**DECIMALS_OFFSET, virtualAssets = 1. The
///      conversion formula's +offset on numerator and denominator dilutes attacker-controlled
///      first deposits and bounds the donation-attack impact.
abstract contract ERC20Share {
    string public name;
    string public symbol;
    uint8  public immutable shareDecimals;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 amount);
    event Approval(address indexed owner, address indexed spender, uint256 amount);

    constructor(string memory _name, string memory _symbol, uint8 _decimals) {
        name = _name;
        symbol = _symbol;
        shareDecimals = _decimals;
    }

    function _mint(address to, uint256 amount) internal {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function _burn(address from, uint256 amount) internal {
        balanceOf[from] -= amount;
        totalSupply    -= amount;
        emit Transfer(from, address(0), amount);
    }

    function _spendAllowance(address owner_, address spender, uint256 amount) internal {
        if (owner_ != spender) {
            uint256 allowed = allowance[owner_][spender];
            if (allowed != type(uint256).max) {
                require(allowed >= amount, "InsufficientAllowance");
                allowance[owner_][spender] = allowed - amount;
            }
        }
    }
}

contract ExampleVault is ERC20Share {
    IERC20 public immutable asset;
    address public owner;

    uint16  public feeBps;            // fee on yield in basis points (10000 = 100%)
    address public feeRecipient;

    uint256 private _totalAssets;     // total underlying asset balance under management

    /// @dev Virtual-offset inflation defense.
    /// virtualShares = 10 ** DECIMALS_OFFSET, virtualAssets = 1.
    /// Larger offset = stronger defense (more rate dilution for attacker's seeding deposit)
    /// at the cost of dust precision at very small supply.
    uint8 public constant DECIMALS_OFFSET = 6;

    event Deposit(address indexed sender, address indexed receiver, uint256 assets, uint256 shares);
    event Withdraw(address indexed sender, address indexed receiver, address indexed owner_, uint256 assets, uint256 shares);
    event Earn(uint256 grossYield, uint256 feeShares);
    event FeeBpsUpdated(uint16 oldBps, uint16 newBps);
    event FeeRecipientUpdated(address oldRecipient, address newRecipient);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    error NotOwner();
    error ZeroAssets();
    error ZeroShares();
    error InvalidFee();
    error InsufficientLiquidity();
    error ZeroAddress();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address _asset, uint16 _feeBps, address _feeRecipient)
        ERC20Share("VaultShare", "vSHR", IERC20(_asset).decimals() + DECIMALS_OFFSET)
    {
        if (_asset == address(0) || _feeRecipient == address(0)) revert ZeroAddress();
        if (_feeBps > 10000) revert InvalidFee();
        asset = IERC20(_asset);
        feeBps = _feeBps;
        feeRecipient = _feeRecipient;
        owner = msg.sender;
        emit OwnershipTransferred(address(0), msg.sender);
    }

    // ---- ERC-4626 views ----

    function totalAssets() public view returns (uint256) { return _totalAssets; }

    function _virtualShares() internal pure returns (uint256) { return 10 ** DECIMALS_OFFSET; }
    function _virtualAssets() internal pure returns (uint256) { return 1; }

    /// @dev assets → shares, ROUND DOWN (favors vault). Used by deposit and convertToShares.
    function convertToShares(uint256 assets) public view returns (uint256) {
        return (assets * (totalSupply + _virtualShares())) / (_totalAssets + _virtualAssets());
    }

    /// @dev shares → assets, ROUND DOWN (favors vault). Used by redeem and convertToAssets.
    function convertToAssets(uint256 shares) public view returns (uint256) {
        return (shares * (_totalAssets + _virtualAssets())) / (totalSupply + _virtualShares());
    }

    function previewDeposit(uint256 assets) public view returns (uint256) { return convertToShares(assets); }
    function previewRedeem(uint256 shares) public view returns (uint256) { return convertToAssets(shares); }

    /// @dev shares → assets needed to mint, ROUND UP (user pays a bit more, favors vault).
    function previewMint(uint256 shares) public view returns (uint256) {
        uint256 num = shares * (_totalAssets + _virtualAssets());
        uint256 den = totalSupply + _virtualShares();
        return (num + den - 1) / den;
    }

    /// @dev assets → shares to burn, ROUND UP (user burns a bit more, favors vault).
    function previewWithdraw(uint256 assets) public view returns (uint256) {
        uint256 num = assets * (totalSupply + _virtualShares());
        uint256 den = _totalAssets + _virtualAssets();
        return (num + den - 1) / den;
    }

    // ---- ERC-4626 actions ----

    function deposit(uint256 assets, address receiver) external returns (uint256 shares) {
        if (assets == 0) revert ZeroAssets();
        shares = previewDeposit(assets);
        if (shares == 0) revert ZeroShares();

        _totalAssets += assets;
        _mint(receiver, shares);
        require(asset.transferFrom(msg.sender, address(this), assets), "TransferFromFailed");

        emit Deposit(msg.sender, receiver, assets, shares);
    }

    function mint(uint256 shares, address receiver) external returns (uint256 assets) {
        if (shares == 0) revert ZeroShares();
        assets = previewMint(shares);

        _totalAssets += assets;
        _mint(receiver, shares);
        require(asset.transferFrom(msg.sender, address(this), assets), "TransferFromFailed");

        emit Deposit(msg.sender, receiver, assets, shares);
    }

    function withdraw(uint256 assets, address receiver, address owner_) external returns (uint256 shares) {
        if (assets == 0) revert ZeroAssets();
        if (assets > _totalAssets) revert InsufficientLiquidity();
        shares = previewWithdraw(assets);

        _spendAllowance(owner_, msg.sender, shares);
        _burn(owner_, shares);
        _totalAssets -= assets;
        require(asset.transfer(receiver, assets), "TransferFailed");

        emit Withdraw(msg.sender, receiver, owner_, assets, shares);
    }

    function redeem(uint256 shares, address receiver, address owner_) external returns (uint256 assets) {
        if (shares == 0) revert ZeroShares();
        assets = previewRedeem(shares);
        if (assets > _totalAssets) revert InsufficientLiquidity();

        _spendAllowance(owner_, msg.sender, shares);
        _burn(owner_, shares);
        _totalAssets -= assets;
        require(asset.transfer(receiver, assets), "TransferFailed");

        emit Withdraw(msg.sender, receiver, owner_, assets, shares);
    }

    // ---- Yield realization ----

    /// @notice Realize gross `yield` of underlying. In production a keeper or a strategy
    ///         contract pushes here after redeeming from the lending venue. For the example
    ///         the caller transfers `yield` underlying tokens to the vault and we mint fee
    ///         shares to feeRecipient at the *pre-yield* price.
    /// @dev Owner-gated for simplicity; in production this is callable by the strategy.
    function _earn(uint256 yield) external onlyOwner returns (uint256 feeShares) {
        if (yield == 0) return 0;

        if (feeBps > 0 && totalSupply > 0) {
            // Fee in asset units, taken from gross yield.
            uint256 feeAssets = (yield * feeBps) / 10000;
            // Mint shares to feeRecipient at the pre-yield price = totalAssets / totalSupply.
            // Using the same +offset formula so the math is consistent with deposits.
            feeShares = (feeAssets * (totalSupply + _virtualShares())) / (_totalAssets + _virtualAssets());
            _mint(feeRecipient, feeShares);
        }

        _totalAssets += yield;
        require(asset.transferFrom(msg.sender, address(this), yield), "TransferFromFailed");

        emit Earn(yield, feeShares);
    }

    // ---- Admin ----

    function setFeeBps(uint16 newBps) external onlyOwner {
        if (newBps > 10000) revert InvalidFee();
        uint16 old = feeBps;
        feeBps = newBps;
        emit FeeBpsUpdated(old, newBps);
    }

    function setFeeRecipient(address newRecipient) external onlyOwner {
        if (newRecipient == address(0)) revert ZeroAddress();
        address old = feeRecipient;
        feeRecipient = newRecipient;
        emit FeeRecipientUpdated(old, newRecipient);
    }

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        address prev = owner;
        owner = newOwner;
        emit OwnershipTransferred(prev, newOwner);
    }
}
