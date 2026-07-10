// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

/// @title MockERC20
/// @notice A freely mintable ERC-20 with EIP-2612 permit. Demo collateral for the DuBu
///         proprietary AMM on GIWA Sepolia.
///
/// GIWA Sepolia has no canonical stablecoin. Circle publishes no CCTP domain for chain 91342,
/// USDT0 does not list it, and every "USDC" on the explorer is somebody else's mock. The only
/// asset that arrives honestly is ETH, and the faucet caps that at 0.015/day. A demo built on
/// bridged assets would therefore top out at a few dollars of TVL — which is not a pool, it is
/// a rounding error, and it makes every realized-spread curve we want to show meaningless.
///
/// So both sides of every demo pair are this contract. Supply is not a constraint, pool depth
/// becomes a deployment parameter, and the size sweep can run out to notionals that actually
/// exercise the curve's linear impact term.
///
/// `decimals` is a constructor argument rather than a constant on purpose. PropPool's
/// `priceScaleExp` — the per-pair decimal alignment factor that `PropCurve.amountOutBid` divides
/// by and `amountOutAsk` multiplies by — is the identity when both tokens share a decimals
/// value, so a demo built entirely from 18-decimal tokens never executes that code path at all.
/// The deployment mixes 18-, 8- and 6-decimal tokens specifically to force it.
///
/// @dev ⚠️ TESTNET PROP — NEVER DEPLOY TO MAINNET. ⚠️
///      `mint` has no access control of any kind: any address, at any time, can create an
///      unbounded amount of this token for any recipient. Anything holding it as collateral,
///      pricing it, or lending against it can be drained for the cost of one transaction. This
///      contract exists to make a testnet demo unconstrained by faucets; it has no other use,
///      and it must never appear in a deployment script that targets a chain with real value on
///      it. If you are reading this while writing a mainnet script, you are in the wrong file.
contract MockERC20 {
    // ---------------------------------------------------------------------
    // Errors
    // ---------------------------------------------------------------------

    error InsufficientBalance();
    error InsufficientAllowance();
    error InvalidReceiver();
    error InvalidSpender();
    error PermitExpired();
    error InvalidPermitSignature();

    // ---------------------------------------------------------------------
    // Events
    // ---------------------------------------------------------------------

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    // ---------------------------------------------------------------------
    // Metadata
    // ---------------------------------------------------------------------

    string public name;
    string public symbol;

    /// @notice Set once at construction. See the contract note on `priceScaleExp`.
    /// @dev Immutable rather than storage: `decimals()` is read on every quote an aggregator
    ///      simulates locally, and the pool reads it once per pair at registration. The name is
    ///      fixed by ERC-20 and cannot be respelled to satisfy the immutable-casing lint.
    // forge-lint: disable-next-line(screaming-snake-case-immutable)
    uint8 public immutable decimals;

    // ---------------------------------------------------------------------
    // Ledger
    // ---------------------------------------------------------------------

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    // ---------------------------------------------------------------------
    // EIP-2612
    // ---------------------------------------------------------------------

    /// @notice `keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")`
    bytes32 public constant PERMIT_TYPEHASH =
        keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)");

    bytes32 private constant _EIP712_DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    /// @notice Signature replay counter, one per owner. Consumed by `permit`.
    mapping(address => uint256) public nonces;

    /// @dev The domain separator is fixed for the life of the contract *given a fixed chain id*,
    ///      so it is computed once and burned into the bytecode. `block.chainid` is the one input
    ///      that can change underneath a deployed contract — a chain fork produces two chains
    ///      sharing this address and this code, and a separator cached across that boundary would
    ///      make every signature valid on both. Re-deriving on mismatch keeps signatures bound to
    ///      the chain they were signed for. It is also what makes `vm.chainId()` work in tests.
    uint256 private immutable _CACHED_CHAIN_ID;
    bytes32 private immutable _CACHED_DOMAIN_SEPARATOR;

    // ---------------------------------------------------------------------
    // Construction
    // ---------------------------------------------------------------------

    constructor(string memory name_, string memory symbol_, uint8 decimals_) {
        name = name_;
        symbol = symbol_;
        decimals = decimals_;

        // `name` must already be in storage: it is hashed into the domain.
        _CACHED_CHAIN_ID = block.chainid;
        _CACHED_DOMAIN_SEPARATOR = _computeDomainSeparator();
    }

    // ---------------------------------------------------------------------
    // ERC-20
    // ---------------------------------------------------------------------

    function transfer(address to, uint256 amount) public virtual returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) public virtual returns (bool) {
        _spendAllowance(from, msg.sender, amount);
        _transfer(from, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) public virtual returns (bool) {
        _approve(msg.sender, spender, amount);
        return true;
    }

    // ---------------------------------------------------------------------
    // Supply — unrestricted by design, see the contract-level warning
    // ---------------------------------------------------------------------

    /// @notice Create `amount` of this token out of nothing and credit it to `to`.
    /// @dev No caller check. This is the whole point of the contract: the demo seeds pool
    ///      inventory, funds visitor wallets, and refills a drained side of a pair without ever
    ///      touching a faucet.
    function mint(address to, uint256 amount) public virtual {
        _mint(to, amount);
    }

    /// @notice Destroy `amount` of the caller's own balance.
    function burn(uint256 amount) public virtual {
        _burn(msg.sender, amount);
    }

    /// @notice Destroy `amount` of `from`'s balance, drawing on the caller's allowance.
    /// @dev Deliberately *not* a free-for-all, unlike `mint`. Unrestricted minting can only add
    ///      to someone's position; unrestricted burning would let any passer-by zero out the
    ///      pool's reserves mid-demo and make the AMM look broken for reasons that have nothing
    ///      to do with the AMM. The allowance check costs nothing and closes that.
    function burnFrom(address from, uint256 amount) public virtual {
        _spendAllowance(from, msg.sender, amount);
        _burn(from, amount);
    }

    // ---------------------------------------------------------------------
    // EIP-2612
    // ---------------------------------------------------------------------

    /// @notice EIP-712 domain separator for this token on the *current* chain.
    /// @dev The screaming-snake name is mandated by EIP-2612; every wallet and permit helper
    ///      calls this exact selector, so the mixed-case lint has to be silenced rather than
    ///      obeyed.
    // forge-lint: disable-next-line(mixed-case-function)
    function DOMAIN_SEPARATOR() public view returns (bytes32) {
        return block.chainid == _CACHED_CHAIN_ID ? _CACHED_DOMAIN_SEPARATOR : _computeDomainSeparator();
    }

    /// @notice Set `spender`'s allowance over `owner`'s tokens from an off-chain signature.
    /// @dev Lets the demo frontend do approve+swap in a single transaction, which matters on a
    ///      chain where visitors are rationing 0.015 ETH/day of gas.
    function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s)
        public
        virtual
    {
        if (block.timestamp > deadline) revert PermitExpired();

        // The post-increment is inside the digest: the signature is bound to exactly one nonce,
        // and consuming it here makes a second submission recover a different address.
        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                DOMAIN_SEPARATOR(),
                keccak256(abi.encode(PERMIT_TYPEHASH, owner, spender, value, nonces[owner]++, deadline))
            )
        );

        address recovered = ecrecover(digest, v, r, s);
        // `ecrecover` returns address(0) for a malformed signature, so an `owner` of address(0)
        // would otherwise match anything. Check both halves.
        if (recovered == address(0) || recovered != owner) revert InvalidPermitSignature();

        _approve(owner, spender, value);
    }

    function _computeDomainSeparator() internal view returns (bytes32) {
        return keccak256(
            abi.encode(_EIP712_DOMAIN_TYPEHASH, keccak256(bytes(name)), keccak256("1"), block.chainid, address(this))
        );
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    function _transfer(address from, address to, uint256 amount) internal virtual {
        // ERC-20 does not require this, but a demo that silently eats a mis-addressed transfer
        // into the zero address is strictly worse than one that reverts in front of the visitor.
        if (to == address(0)) revert InvalidReceiver();

        uint256 fromBalance = balanceOf[from];
        if (fromBalance < amount) revert InsufficientBalance();

        unchecked {
            balanceOf[from] = fromBalance - amount;
            // Cannot overflow: the sum of all balances equals `totalSupply`, and the only place
            // `totalSupply` grows is `_mint`, where the addition is checked.
            balanceOf[to] += amount;
        }

        emit Transfer(from, to, amount);
    }

    function _mint(address to, uint256 amount) internal virtual {
        if (to == address(0)) revert InvalidReceiver();

        // Checked on purpose — this is the only overflow boundary in the ledger.
        totalSupply += amount;
        unchecked {
            balanceOf[to] += amount;
        }

        emit Transfer(address(0), to, amount);
    }

    function _burn(address from, uint256 amount) internal virtual {
        uint256 fromBalance = balanceOf[from];
        if (fromBalance < amount) revert InsufficientBalance();

        unchecked {
            balanceOf[from] = fromBalance - amount;
            // `amount <= balanceOf[from] <= totalSupply`, so this cannot underflow.
            totalSupply -= amount;
        }

        emit Transfer(from, address(0), amount);
    }

    function _approve(address owner, address spender, uint256 amount) internal virtual {
        if (spender == address(0)) revert InvalidSpender();
        allowance[owner][spender] = amount;
        emit Approval(owner, spender, amount);
    }

    /// @dev `type(uint256).max` is an infinite approval and is never decremented. This is the
    ///      near-universal convention and it saves an SSTORE on every hop of a routed swap,
    ///      which is where the pool's own gas budget lives. No `Approval` event is emitted for
    ///      the decrement: it is not required by the standard and indexers do not rely on it.
    function _spendAllowance(address owner, address spender, uint256 amount) internal virtual {
        uint256 allowed = allowance[owner][spender];
        if (allowed == type(uint256).max) return;
        if (allowed < amount) revert InsufficientAllowance();
        unchecked {
            allowance[owner][spender] = allowed - amount;
        }
    }
}
