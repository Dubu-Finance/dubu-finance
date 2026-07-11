// SPDX-License-Identifier: GPL-3.0
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {UniswapV2Factory} from "./UniswapV2Factory.sol";
import {UniswapV2Pair} from "./UniswapV2Pair.sol";
import {UniswapV2Router02} from "./UniswapV2Router02.sol";
import {IUniswapV2Pair} from "./interfaces/IUniswapV2Pair.sol";
import {UniswapV2Library} from "./libraries/UniswapV2Library.sol";

contract TestERC20 {
    string public name = "T";
    string public symbol = "T";
    uint8 public decimals = 18;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    function approve(address s, uint256 v) external returns (bool) {
        allowance[msg.sender][s] = v;
        return true;
    }

    function transfer(address t, uint256 v) external returns (bool) {
        _t(msg.sender, t, v);
        return true;
    }

    function transferFrom(address f, address t, uint256 v) external returns (bool) {
        if (allowance[f][msg.sender] != type(uint256).max) allowance[f][msg.sender] -= v;
        _t(f, t, v);
        return true;
    }

    function _t(address f, address t, uint256 v) private {
        balanceOf[f] -= v;
        balanceOf[t] += v;
        emit Transfer(f, t, v);
    }

    function mint(address t, uint256 v) external {
        totalSupply += v;
        balanceOf[t] += v;
        emit Transfer(address(0), t, v);
    }
}

contract PortVerification is Test {
    UniswapV2Factory factory;
    TestERC20 tA;
    TestERC20 tB;
    UniswapV2Pair pair;
    address token0;
    address token1;

    address other = address(0xB0B);

    function setUp() public {
        factory = new UniswapV2Factory(address(this));
        tA = new TestERC20();
        tB = new TestERC20();
        pair = UniswapV2Pair(factory.createPair(address(tA), address(tB)));
        token0 = pair.token0();
        token1 = pair.token1();
    }

    function _addLiquidity(uint256 a0, uint256 a1) internal {
        TestERC20(token0).mint(address(pair), a0);
        TestERC20(token1).mint(address(pair), a1);
        pair.mint(address(this));
    }

    // ---------------------------------------------------------------------------------
    // Upstream v2-core test/UniswapV2Pair.spec.ts :: swapTestCases ("getInputPrice")
    // [swapAmount, token0Amount, token1Amount, expectedOutputAmount]
    // These are the canonical constant-product fixtures. Reproducing them bit-exactly is
    // the strongest single evidence that the 0.3% fee + k check survived the port.
    // ---------------------------------------------------------------------------------
    function testUpstreamGetInputPriceVectors() public {
        uint256[4][7] memory cases = [
            [uint256(1e18), 5e18, 10e18, 1662497915624478906],
            [uint256(1e18), 10e18, 5e18, 453305446940074565],
            [uint256(2e18), 5e18, 10e18, 2851015155847869602],
            [uint256(2e18), 10e18, 5e18, 831248957812239453],
            [uint256(1e18), 10e18, 10e18, 906610893880149131],
            [uint256(1e18), 100e18, 100e18, 987158034397061298],
            [uint256(1e18), 1000e18, 1000e18, 996006981039903216]
        ];

        for (uint256 i = 0; i < cases.length; i++) {
            setUp();
            uint256 swapAmount = cases[i][0];
            uint256 r0 = cases[i][1];
            uint256 r1 = cases[i][2];
            uint256 expected = cases[i][3];

            _addLiquidity(r0, r1);
            TestERC20(token0).mint(address(pair), swapAmount);

            // one wei more than the curve allows must break the k invariant
            vm.expectRevert(bytes("UniswapV2: K"));
            pair.swap(0, expected + 1, address(this), new bytes(0));

            // exactly the curve amount must succeed
            pair.swap(0, expected, address(this), new bytes(0));

            // and the pure library quote must agree with what the pair accepted
            assertEq(UniswapV2Library.getAmountOut(swapAmount, r0, r1), expected, "library != pair");
        }
    }

    // Upstream v2-core :: optimisticTestCases
    // [outputAmount, token0Amount, token1Amount, inputAmount]
    function testUpstreamOptimisticVectors() public {
        uint256[4][4] memory cases = [
            [uint256(997000000000000000), 5e18, 10e18, 1e18],
            [uint256(997000000000000000), 10e18, 5e18, 1e18],
            [uint256(997000000000000000), 5e18, 5e18, 1e18],
            [uint256(1e18), 5e18, 5e18, 1003009027081243732]
        ];

        for (uint256 i = 0; i < cases.length; i++) {
            setUp();
            uint256 outputAmount = cases[i][0];
            uint256 r0 = cases[i][1];
            uint256 r1 = cases[i][2];
            uint256 inputAmount = cases[i][3];

            _addLiquidity(r0, r1);
            TestERC20(token0).mint(address(pair), inputAmount);

            vm.expectRevert(bytes("UniswapV2: K"));
            pair.swap(outputAmount + 1, 0, address(this), new bytes(0));

            pair.swap(outputAmount, 0, address(this), new bytes(0));
        }
    }

    // Upstream v2-core :: "mint" — 1:4 deposit mints sqrt(k) - MINIMUM_LIQUIDITY
    function testUpstreamMint() public {
        _addLiquidity(1e18, 4e18);
        assertEq(pair.totalSupply(), 2e18);
        assertEq(pair.balanceOf(address(this)), 2e18 - 1000);
        assertEq(pair.balanceOf(address(0)), 1000, "MINIMUM_LIQUIDITY not locked");
        assertEq(pair.MINIMUM_LIQUIDITY(), 1000);
    }

    // Upstream v2-core :: "feeTo:on" — protocol fee is 1/6 of sqrt(k) growth
    function testUpstreamFeeToOn() public {
        factory.setFeeTo(other);
        _addLiquidity(1000e18, 1000e18);

        uint256 expectedOutputAmount = 996006981039903216;
        TestERC20(token1).mint(address(pair), 1e18);
        pair.swap(expectedOutputAmount, 0, address(this), new bytes(0));

        // burn all of our LP
        pair.transfer(address(pair), pair.balanceOf(address(this)));
        pair.burn(address(this));

        assertEq(pair.totalSupply(), 1000 + 249750499251388, "protocol fee mint mismatch");
        assertEq(pair.balanceOf(other), 249750499251388, "feeTo balance mismatch");
    }

    // Upstream v2-core :: "feeTo:off" — no protocol fee when feeTo is unset
    function testUpstreamFeeToOff() public {
        _addLiquidity(1000e18, 1000e18);
        uint256 expectedOutputAmount = 996006981039903216;
        TestERC20(token1).mint(address(pair), 1e18);
        pair.swap(expectedOutputAmount, 0, address(this), new bytes(0));
        pair.transfer(address(pair), pair.balanceOf(address(this)));
        pair.burn(address(this));
        assertEq(pair.totalSupply(), 1000, "fee minted while feeTo was off");
    }

    // ---------------------------------------------------------------------------------
    // TWAP / unchecked semantics. These are the tests that catch the classic 0.8 port bug.
    // ---------------------------------------------------------------------------------

    function testPriceAccumulatorAdvances() public {
        _addLiquidity(3e18, 3e18);
        (,, uint32 t0) = pair.getReserves();

        vm.warp(block.timestamp + 10);
        pair.sync();

        // 1:1 reserves => price is exactly 2**112 per second
        assertEq(pair.price0CumulativeLast(), (uint256(2) ** 112) * 10);
        assertEq(pair.price1CumulativeLast(), (uint256(2) ** 112) * 10);
        (,, uint32 t1) = pair.getReserves();
        assertEq(t1 - t0, 10);
    }

    /// The uint32 timestamp wraps roughly every 136 years. Upstream relies on the
    /// subtraction underflowing to still yield the right elapsed time. Under checked
    /// arithmetic this reverts and bricks the pair forever. This test fails loudly if the
    /// `unchecked` block in `_update` is ever removed or narrowed.
    function testTimestampWrapDoesNotBrickPair() public {
        uint256 boundary = 2 ** 32;

        vm.warp(boundary - 100);
        _addLiquidity(3e18, 3e18);
        (,, uint32 tBefore) = pair.getReserves();
        assertEq(tBefore, uint32(boundary - 100));

        uint256 accBefore = pair.price0CumulativeLast();

        // cross the 2**32 boundary: 150 seconds of real time, but the uint32 clock wraps
        vm.warp(boundary + 50);
        pair.sync(); // <-- would revert with Panic(0x11) if `unchecked` were dropped

        (,, uint32 tAfter) = pair.getReserves();
        assertEq(tAfter, 50, "timestamp did not wrap as upstream");
        assertEq(
            pair.price0CumulativeLast() - accBefore,
            (uint256(2) ** 112) * 150,
            "wrapped timeElapsed produced the wrong accumulator delta"
        );
    }

    /// The accumulators are defined modulo 2**256 and MUST wrap on overflow; every V2 TWAP
    /// oracle takes a difference of two readings and relies on that.
    function testPriceAccumulatorOverflowWraps() public {
        // maximally skewed reserves => maximal price per second
        uint256 big = 2 ** 112 - 1;
        _addLiquidity(1, big);

        vm.warp(block.timestamp + (2 ** 32 - 1));
        pair.sync();
        uint256 acc1 = pair.price0CumulativeLast();
        assertGt(acc1, 0);

        vm.warp(block.timestamp + (2 ** 32 - 1));
        pair.sync(); // <-- would revert with Panic(0x11) if `unchecked` were dropped
        uint256 acc2 = pair.price0CumulativeLast();

        assertLt(acc2, acc1, "accumulator did not wrap; unchecked semantics lost");
    }

    /// Sanity-check the bound asserted in UQ112x112's port note and in `_update`:
    /// max UQ value * max timeElapsed provably fits in uint256.
    function testUqTimesTimeElapsedCannotOverflow() public pure {
        uint256 maxUq = (2 ** 112 - 1) * 2 ** 112; // == 2**224 - 2**112
        uint256 maxElapsed = 2 ** 32 - 1;
        unchecked {
            uint256 product = maxUq * maxElapsed;
            assert(product / maxElapsed == maxUq); // no wrap occurred
        }
    }

    // ---------------------------------------------------------------------------------
    // Factory + Library (the getPair-instead-of-CREATE2 deviation)
    // ---------------------------------------------------------------------------------

    function testFactoryBookkeeping() public view {
        assertEq(factory.allPairsLength(), 1);
        assertEq(factory.getPair(address(tA), address(tB)), address(pair));
        assertEq(factory.getPair(address(tB), address(tA)), address(pair), "reverse mapping missing");
        assertEq(factory.allPairs(0), address(pair));
        assertEq(pair.factory(), address(factory));
    }

    function testCreatePairIsCreate2Deterministic() public view {
        (address t0, address t1) =
            address(tA) < address(tB) ? (address(tA), address(tB)) : (address(tB), address(tA));
        address expected = address(
            uint160(
                uint256(
                    keccak256(
                        abi.encodePacked(
                            hex"ff",
                            address(factory),
                            keccak256(abi.encodePacked(t0, t1)),
                            factory.pairInitCodeHash()
                        )
                    )
                )
            )
        );
        assertEq(expected, address(pair), "CREATE2 derivation broken");
    }

    function testLibraryPairForRevertsOnMissingPair() public {
        TestERC20 tC = new TestERC20();
        vm.expectRevert(bytes("UniswapV2Library: PAIR_NOT_FOUND"));
        this.probePairFor(address(tA), address(tC));
    }

    function probePairFor(address a, address b) external view returns (address) {
        return UniswapV2Library.pairFor(address(factory), a, b);
    }

    // ---------------------------------------------------------------------------------
    // Router end-to-end
    // ---------------------------------------------------------------------------------

    function testRouterSwapMatchesQuote() public {
        UniswapV2Router02 router = new UniswapV2Router02(address(factory), address(0xdead));
        _addLiquidity(1000e18, 1000e18);

        uint256 amountIn = 1e18;
        TestERC20(token0).mint(address(this), amountIn);
        TestERC20(token0).approve(address(router), type(uint256).max);

        address[] memory path = new address[](2);
        path[0] = token0;
        path[1] = token1;

        uint256[] memory quoted = router.getAmountsOut(amountIn, path);
        assertEq(quoted[1], 996006981039903216, "router quote != upstream vector");

        uint256 before = TestERC20(token1).balanceOf(address(this));
        uint256[] memory got = router.swapExactTokensForTokens(amountIn, 0, path, address(this), block.timestamp);
        assertEq(got[1], quoted[1]);
        assertEq(TestERC20(token1).balanceOf(address(this)) - before, quoted[1], "fill != quote");
    }

    function testRouterExactOutRoundsInFavourOfPool() public {
        UniswapV2Router02 router = new UniswapV2Router02(address(factory), address(0xdead));
        _addLiquidity(1000e18, 1000e18);

        address[] memory path = new address[](2);
        path[0] = token0;
        path[1] = token1;

        uint256 amountOut = 1e18;
        uint256[] memory amounts = router.getAmountsIn(amountOut, path);
        // getAmountIn adds the +1 ceiling term; check it against the closed form
        uint256 expected = (1000e18 * amountOut * 1000) / ((1000e18 - amountOut) * 997) + 1;
        assertEq(amounts[0], expected, "getAmountIn ceiling lost");

        TestERC20(token0).mint(address(this), amounts[0]);
        TestERC20(token0).approve(address(router), type(uint256).max);
        router.swapTokensForExactTokens(amountOut, amounts[0], path, address(this), block.timestamp);
    }

    function testRouterAddAndRemoveLiquidity() public {
        UniswapV2Router02 router = new UniswapV2Router02(address(factory), address(0xdead));
        tA.mint(address(this), 10e18);
        tB.mint(address(this), 10e18);
        tA.approve(address(router), type(uint256).max);
        tB.approve(address(router), type(uint256).max);

        (,, uint256 liq) =
            router.addLiquidity(address(tA), address(tB), 4e18, 4e18, 0, 0, address(this), block.timestamp);
        assertEq(liq, 4e18 - 1000);

        pair.approve(address(router), type(uint256).max);
        (uint256 a, uint256 b) =
            router.removeLiquidity(address(tA), address(tB), liq, 0, 0, address(this), block.timestamp);
        assertEq(a, 4e18 - 1000);
        assertEq(b, 4e18 - 1000);
    }

    function testRouterImmutablesAreReadable() public {
        UniswapV2Router02 router = new UniswapV2Router02(address(factory), address(0xdead));
        assertEq(router.factory(), address(factory));
        assertEq(router.WETH(), address(0xdead));
    }

    // ---------------------------------------------------------------------------------
    // ERC20 / permit surface
    // ---------------------------------------------------------------------------------

    function testPermitTypehashAndDomainSeparator() public view {
        assertEq(
            pair.PERMIT_TYPEHASH(),
            keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
        );
        assertEq(
            pair.DOMAIN_SEPARATOR(),
            keccak256(
                abi.encode(
                    keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                    keccak256(bytes("Uniswap V2")),
                    keccak256(bytes("1")),
                    block.chainid,
                    address(pair)
                )
            )
        );
        assertEq(pair.name(), "Uniswap V2");
        assertEq(pair.symbol(), "UNI-V2");
        assertEq(pair.decimals(), 18);
    }

    function testPermit() public {
        uint256 pk = 0xA11CE;
        address owner = vm.addr(pk);
        _addLiquidity(4e18, 4e18);
        pair.transfer(owner, 1e18);

        bytes32 digest = keccak256(
            abi.encodePacked(
                "\x19\x01",
                pair.DOMAIN_SEPARATOR(),
                keccak256(
                    abi.encode(pair.PERMIT_TYPEHASH(), owner, other, uint256(1e18), uint256(0), block.timestamp)
                )
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(pk, digest);
        pair.permit(owner, other, 1e18, block.timestamp, v, r, s);
        assertEq(pair.allowance(owner, other), 1e18);
        assertEq(pair.nonces(owner), 1);
    }

    function testInfiniteAllowanceIsNotDecremented() public {
        _addLiquidity(4e18, 4e18);
        pair.approve(other, type(uint256).max);
        vm.prank(other);
        pair.transferFrom(address(this), other, 1e18);
        assertEq(pair.allowance(address(this), other), type(uint256).max);
    }

    // ---------------------------------------------------------------------------------
    // Guards
    // ---------------------------------------------------------------------------------

    function testReserveOverflowGuard() public {
        _addLiquidity(1e18, 1e18);
        TestERC20(token0).mint(address(pair), 2 ** 112);
        vm.expectRevert(bytes("UniswapV2: OVERFLOW"));
        pair.sync();
    }

    function testSwapToTokenAddressRejected() public {
        _addLiquidity(5e18, 5e18);
        TestERC20(token0).mint(address(pair), 1e18);
        vm.expectRevert(bytes("UniswapV2: INVALID_TO"));
        pair.swap(0, 1e17, token1, new bytes(0));
    }

    function testSkim() public {
        _addLiquidity(5e18, 5e18);
        TestERC20(token0).mint(address(pair), 7);
        pair.skim(other);
        assertEq(TestERC20(token0).balanceOf(other), 7);
    }

    function testInitializeOnlyFactory() public {
        vm.expectRevert(bytes("UniswapV2: FORBIDDEN"));
        pair.initialize(address(tA), address(tB));
    }
}
