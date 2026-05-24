// SPDX-License-Identifier: MIT
pragma solidity ^0.8.34;

// Simplified AMM (constant-product, x·y = k) with liquidity pools.
// Follows the Uniswap V2 model: https://github.com/Uniswap/v2-core
// Key differences from real V2: no ERC20 LP token, no protocol fee, no TWAP oracle,
// no minimum liquidity lock, no token-sorting by address.
//
// Evolution of on-chain atomic swaps (most advanced approaches as of 2025-26):
//
//  Uniswap V4 (2024)      — singleton pool contract + "hooks" (arbitrary logic before/after
//                           every swap or liquidity event); flash accounting via EIP-1153
//                           transient storage defers all balance checks to end of tx.
//                           https://github.com/Uniswap/v4-core
//
//  Intent-based protocols — user signs a declarative order ("I want ≥X out for Y in by T");
//  UniswapX / CoW / Fusion  off-chain solvers compete to fill it, settle on-chain atomically.
//                           Eliminates direct AMM interaction; MEV captured by solvers, not
//                           validators. CoW uses batch auctions; UniswapX uses Dutch auctions.
//                           https://github.com/Uniswap/UniswapX
//
//  ERC-7683 (2024)        — cross-chain intent standard; same signed order settled on any
//                           supported chain by a permissionless filler network (Across, etc.).
//                           True atomic cross-chain swaps without bridges holding funds.
//                           https://eips.ethereum.org/EIPS/eip-7683
//
//  TWAMM                  — Time-Weighted AMM; splits a large order into infinitely many
//                           infinitesimal virtual swaps over a time window, minimising price
//                           impact for long-duration trades. A natural Uni V4 hook.
//                           https://www.paradigm.xyz/2021/07/twamm

interface IERC20 {
    function transfer(address to, uint amount) external returns (bool);
    function transferFrom(address from, address to, uint amount) external returns (bool);
}

contract Maker {
    IERC20 public immutable tokenA;
    IERC20 public immutable tokenB;

    uint public reserveA;
    uint public reserveB;
    uint public totalShares;
    mapping(address => uint) public shares;

    uint private constant FEE = 997; // 0.3% fee (997/1000)

    constructor(IERC20 _tokenA, IERC20 _tokenB) {
        tokenA = _tokenA;
        tokenB = _tokenB;
    }

    // Deposit both tokens proportionally; first deposit sets the price.
    // Returns LP shares minted (geometric mean of deposits for first LP).
    function addLiquidity(uint amountA, uint amountB) external returns (uint minted) {
        tokenA.transferFrom(msg.sender, address(this), amountA);
        tokenB.transferFrom(msg.sender, address(this), amountB);

        minted = totalShares == 0
            ? sqrt(amountA * amountB)
            : min(amountA * totalShares / reserveA, amountB * totalShares / reserveB);

        require(minted > 0, "zero shares");
        shares[msg.sender] += minted;
        totalShares += minted;
        reserveA += amountA;
        reserveB += amountB;
    }

    // Burn LP shares, withdraw proportional reserves.
    function removeLiquidity(uint amount) external returns (uint outA, uint outB) {
        require(shares[msg.sender] >= amount, "insufficient shares");
        outA = amount * reserveA / totalShares;
        outB = amount * reserveB / totalShares;
        shares[msg.sender] -= amount;
        totalShares -= amount;
        reserveA -= outA;
        reserveB -= outB;
        tokenA.transfer(msg.sender, outA);
        tokenB.transfer(msg.sender, outB);
    }

    // Atomic swap: send tokenIn, receive tokenOut at current pool price minus fee.
    // Caller must pre-approve this contract for amountIn.
    // Price impact: large swaps shift the ratio and get worse rates (slippage).
    function swap(address tokenIn, uint amountIn) external returns (uint amountOut) {
        require(tokenIn == address(tokenA) || tokenIn == address(tokenB), "bad token");
        bool aToB = tokenIn == address(tokenA);

        (IERC20 tIn, IERC20 tOut, uint resIn, uint resOut) = aToB
            ? (tokenA, tokenB, reserveA, reserveB)
            : (tokenB, tokenA, reserveB, reserveA);

        tIn.transferFrom(msg.sender, address(this), amountIn);

        // Constant-product: (resIn + amountIn·fee) · (resOut - amountOut) = resIn · resOut
        uint inWithFee = amountIn * FEE;
        amountOut = inWithFee * resOut / (resIn * 1000 + inWithFee);
        require(amountOut > 0, "zero out");

        tOut.transfer(msg.sender, amountOut);

        if (aToB) { reserveA += amountIn; reserveB -= amountOut; }
        else       { reserveB += amountIn; reserveA -= amountOut; }
    }

    // --- helpers ---

    function sqrt(uint x) private pure returns (uint y) {
        if (x == 0) return 0;
        uint z = (x + 1) / 2;
        y = x;
        while (z < y) { y = z; z = (x / z + z) / 2; }
    }

    function min(uint a, uint b) private pure returns (uint) { return a < b ? a : b; }
}

// Minimal ERC20 for testing the pool without deploying real tokens.
contract MockERC20 {
    mapping(address => uint) public balanceOf;
    mapping(address => mapping(address => uint)) public allowance;

    function mint(address to, uint amount) external { balanceOf[to] += amount; }

    function approve(address spender, uint amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

// End-to-end scenario runner.
//
// simulate() calls run() via try/catch. run() always reverts — so every
// deployment, mint, and trade is rolled back — but the revert payload carries
// the results back to simulate(), which decodes and returns them.
// This is the same trick Uniswap's Quoter uses to price swaps without mutating state.
contract Setup {
    // Packed result surfaced through the revert payload.
    error Result(uint lpShares, uint swapOut, uint finalA, uint finalB);

    // Deploy fresh contracts, run a full trade cycle, then revert everything.
    function run() external {
        // Deploy
        MockERC20 a = new MockERC20();
        MockERC20 b = new MockERC20();
        Maker pool = new Maker(IERC20(address(a)), IERC20(address(b)));

        // Mint
        a.mint(address(this), 1000e18);
        b.mint(address(this), 1000e18);

        // Approve pool to pull tokens
        a.approve(address(pool), type(uint256).max);
        b.approve(address(pool), type(uint256).max);

        // Add liquidity: seed pool with 500/500, price = 1:1
        uint lpShares = pool.addLiquidity(500e18, 500e18);

        // Swap 10 A → B; pool price shifts, B received < 10 due to fee + slippage
        uint swapOut = pool.swap(address(a), 10e18);

        // Withdraw all LP shares
        pool.removeLiquidity(lpShares);

        // Capture final token balances of this contract
        uint finalA = a.balanceOf(address(this));
        uint finalB = b.balanceOf(address(this));

        revert Result(lpShares, swapOut, finalA, finalB);
    }

    // Call run(), intercept the revert, decode and return the results.
    // No state change survives; safe to call on any live network.
    function simulate() external returns (uint lpShares, uint swapOut, uint finalA, uint finalB) {
        try this.run() { revert("no revert"); } catch (bytes memory data) {
            // Guard against unexpected reverts (panics, requires, other custom errors).
            require(data.length == 4 + 4 * 32 && bytes4(data) == Result.selector, "unexpected revert");
            // abi.decode does not accept bytes memory slices (only full arrays or calldata).
            // Read the four words directly from memory instead.
            // Layout: [length @ mem+0x00][selector @ mem+0x20][word0 @ mem+0x24] ...
            assembly {
                lpShares := mload(add(data, 0x24))
                swapOut  := mload(add(data, 0x44))
                finalA   := mload(add(data, 0x64))
                finalB   := mload(add(data, 0x84))
            }
        }
    }
}
