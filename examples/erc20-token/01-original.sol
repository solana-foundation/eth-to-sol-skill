// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title ExampleToken
/// @notice Standard ERC-20 with owner-gated mint, max supply cap, and self-burn.
/// @dev Hand-written for clarity; equivalent to OpenZeppelin ERC20 + Ownable + capped supply.
contract ExampleToken {
    // ---- Metadata ----
    string public name;
    string public symbol;
    uint8 public immutable decimals;

    // ---- Supply ----
    uint256 public totalSupply;
    uint256 public immutable maxSupply;

    // ---- Ownership ----
    address public owner;

    // ---- Balances & allowances ----
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    // ---- Events ----
    event Transfer(address indexed from, address indexed to, uint256 amount);
    event Approval(address indexed owner, address indexed spender, uint256 amount);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    // ---- Errors ----
    error NotOwner();
    error ZeroAddress();
    error MaxSupplyExceeded();
    error InsufficientBalance();
    error InsufficientAllowance();

    // ---- Modifiers ----
    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    // ---- Construction ----
    constructor(
        string memory _name,
        string memory _symbol,
        uint8 _decimals,
        uint256 _maxSupply
    ) {
        name = _name;
        symbol = _symbol;
        decimals = _decimals;
        maxSupply = _maxSupply;
        owner = msg.sender;
        emit OwnershipTransferred(address(0), msg.sender);
    }

    // ---- Core ERC-20 ----

    function transfer(address to, uint256 amount) external returns (bool) {
        if (to == address(0)) revert ZeroAddress();
        if (balanceOf[msg.sender] < amount) revert InsufficientBalance();

        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;

        emit Transfer(msg.sender, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(
        address from,
        address to,
        uint256 amount
    ) external returns (bool) {
        if (to == address(0)) revert ZeroAddress();
        uint256 allowed = allowance[from][msg.sender];
        if (allowed < amount) revert InsufficientAllowance();
        if (balanceOf[from] < amount) revert InsufficientBalance();

        // Infinite-allowance optimization (standard ERC-20 idiom).
        if (allowed != type(uint256).max) {
            allowance[from][msg.sender] = allowed - amount;
        }

        balanceOf[from] -= amount;
        balanceOf[to] += amount;

        emit Transfer(from, to, amount);
        return true;
    }

    // ---- Supply control ----

    function mint(address to, uint256 amount) external onlyOwner {
        if (to == address(0)) revert ZeroAddress();
        if (totalSupply + amount > maxSupply) revert MaxSupplyExceeded();

        totalSupply += amount;
        balanceOf[to] += amount;

        emit Transfer(address(0), to, amount);
    }

    function burn(uint256 amount) external {
        if (balanceOf[msg.sender] < amount) revert InsufficientBalance();

        balanceOf[msg.sender] -= amount;
        totalSupply -= amount;

        emit Transfer(msg.sender, address(0), amount);
    }

    // ---- Ownership ----

    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        address prev = owner;
        owner = newOwner;
        emit OwnershipTransferred(prev, newOwner);
    }
}
