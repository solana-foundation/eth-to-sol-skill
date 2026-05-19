// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title TokenSwapEscrow
/// @notice Two-party atomic ERC-20 swap. The maker locks `amountOffered` of
///         `tokenOffered`; any taker can fulfill by transferring `amountWanted`
///         of `tokenWanted` to the maker and receiving the locked tokens.
///         Neither party can be cheated — both legs settle in one transaction.
/// @dev Mirrors the canonical Solana program-examples escrow shape so the
///      translation walkthrough has a 1:1 reference on the Solana side. There
///      is no single OpenZeppelin contract for ERC-20-for-ERC-20 atomic swaps
///      — this is the standard pattern, using OZ's SafeERC20 wrapper for the
///      transfer-return-value handling.
contract TokenSwapEscrow {
    using SafeERC20 for IERC20;

    struct Offer {
        address maker;
        IERC20 tokenOffered;
        uint256 amountOffered;
        IERC20 tokenWanted;
        uint256 amountWanted;
    }

    /// @notice Auto-incrementing offer id.
    uint256 public nextOfferId;

    /// @notice id => offer. Cleared (zeroed) when taken or cancelled.
    mapping(uint256 => Offer) public offers;

    event OfferMade(
        uint256 indexed id,
        address indexed maker,
        IERC20 tokenOffered,
        uint256 amountOffered,
        IERC20 tokenWanted,
        uint256 amountWanted
    );
    event OfferTaken(uint256 indexed id, address indexed taker);
    event OfferCancelled(uint256 indexed id);

    error ZeroAmount();
    error SameToken();
    error OfferDoesNotExist();
    error NotMaker();

    /// @notice Create an offer. Pulls `amountOffered` of `tokenOffered` from
    ///         the maker; held by this contract until taken or cancelled.
    function makeOffer(
        IERC20 tokenOffered,
        uint256 amountOffered,
        IERC20 tokenWanted,
        uint256 amountWanted
    ) external returns (uint256 id) {
        if (amountOffered == 0 || amountWanted == 0) revert ZeroAmount();
        if (address(tokenOffered) == address(tokenWanted)) revert SameToken();

        id = nextOfferId++;
        offers[id] = Offer({
            maker: msg.sender,
            tokenOffered: tokenOffered,
            amountOffered: amountOffered,
            tokenWanted: tokenWanted,
            amountWanted: amountWanted
        });

        tokenOffered.safeTransferFrom(msg.sender, address(this), amountOffered);

        emit OfferMade(
            id,
            msg.sender,
            tokenOffered,
            amountOffered,
            tokenWanted,
            amountWanted
        );
    }

    /// @notice Fulfil an offer. Pulls `amountWanted` from the taker (sent to
    ///         the maker), then releases `amountOffered` to the taker. Atomic.
    function takeOffer(uint256 id) external {
        Offer memory o = offers[id];
        if (o.maker == address(0)) revert OfferDoesNotExist();
        delete offers[id];

        // Pull the wanted token from the taker, send directly to maker.
        o.tokenWanted.safeTransferFrom(msg.sender, o.maker, o.amountWanted);
        // Release the offered (escrowed) token to the taker.
        o.tokenOffered.safeTransfer(msg.sender, o.amountOffered);

        emit OfferTaken(id, msg.sender);
    }

    /// @notice Maker withdraws their offered tokens before any taker arrives.
    function cancelOffer(uint256 id) external {
        Offer memory o = offers[id];
        if (o.maker == address(0)) revert OfferDoesNotExist();
        if (o.maker != msg.sender) revert NotMaker();
        delete offers[id];

        o.tokenOffered.safeTransfer(o.maker, o.amountOffered);

        emit OfferCancelled(id);
    }
}
