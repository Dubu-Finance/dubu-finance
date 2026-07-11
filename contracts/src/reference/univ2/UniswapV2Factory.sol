// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

import {IUniswapV2Factory} from "./interfaces/IUniswapV2Factory.sol";
import {IUniswapV2Pair} from "./interfaces/IUniswapV2Pair.sol";
import {UniswapV2Pair} from "./UniswapV2Pair.sol";

/// @title UniswapV2Factory
/// @dev Ported from Uniswap V2 core v1.0.1 `contracts/UniswapV2Factory.sol`.
///      https://github.com/Uniswap/v2-core
///
/// PORT NOTES
///   - pragma `=0.5.16` -> `^0.8.28`; `constructor(...) public` -> `constructor(...)`.
///   - CREATE2 with `salt = keccak256(token0, token1)` is kept, so pair addresses are still
///     deterministic. What is NOT kept is the *periphery's* hardcoded init code hash — see
///     `libraries/UniswapV2Library.sol` and README D2. Recompiling with 0.8.28 / prague /
///     optimizer_runs=1_000_000 necessarily changes `type(UniswapV2Pair).creationCode`, so
///     the canonical mainnet hash
///     `0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f` is wrong here.
///   - Upstream does not check the CREATE2 return value; a collision would leave `pair == 0`
///     and the very next call, `IUniswapV2Pair(pair).initialize(...)`, reverts. Left as-is
///     rather than "improved", since `getPair[token0][token1] == 0` is already required above
///     and the salt is a pure function of the (unique) sorted token pair, so a second deploy
///     with the same salt is unreachable.
contract UniswapV2Factory is IUniswapV2Factory {
    address public feeTo;
    address public feeToSetter;

    mapping(address => mapping(address => address)) public getPair;
    address[] public allPairs;

    constructor(address _feeToSetter) {
        feeToSetter = _feeToSetter;
    }

    function allPairsLength() external view returns (uint256) {
        return allPairs.length;
    }

    function createPair(address tokenA, address tokenB) external returns (address pair) {
        require(tokenA != tokenB, "UniswapV2: IDENTICAL_ADDRESSES");
        (address token0, address token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        require(token0 != address(0), "UniswapV2: ZERO_ADDRESS");
        require(getPair[token0][token1] == address(0), "UniswapV2: PAIR_EXISTS"); // single check is sufficient
        bytes memory bytecode = type(UniswapV2Pair).creationCode;
        bytes32 salt = keccak256(abi.encodePacked(token0, token1));
        assembly {
            pair := create2(0, add(bytecode, 32), mload(bytecode), salt)
        }
        IUniswapV2Pair(pair).initialize(token0, token1);
        getPair[token0][token1] = pair;
        getPair[token1][token0] = pair; // populate mapping in the reverse direction
        allPairs.push(pair);
        emit PairCreated(token0, token1, pair, allPairs.length);
    }

    function setFeeTo(address _feeTo) external {
        require(msg.sender == feeToSetter, "UniswapV2: FORBIDDEN");
        feeTo = _feeTo;
    }

    function setFeeToSetter(address _feeToSetter) external {
        require(msg.sender == feeToSetter, "UniswapV2: FORBIDDEN");
        feeToSetter = _feeToSetter;
    }

    /// @notice Init code hash of the freshly compiled `UniswapV2Pair`, for anyone who wants to
    ///         do off-chain CREATE2 derivation against THIS deployment.
    /// @dev Not part of the upstream ABI. Added because the periphery's hardcoded constant is
    ///      unusable for a recompiled port and off-chain tooling still wants the value. It is a
    ///      pure read of a compile-time constant; it does not affect any on-chain code path.
    function pairInitCodeHash() external pure returns (bytes32) {
        return keccak256(type(UniswapV2Pair).creationCode);
    }
}
