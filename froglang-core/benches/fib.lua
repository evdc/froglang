-- Naive recursive fib(35).
-- Run with plain Lua:   lua   fib.lua [n]
-- Run with LuaJIT:      luajit fib.lua [n]
-- (same file works for both)

local function fib(n)
    if n <= 1 then return n end
    return fib(n - 1) + fib(n - 2)
end

-- Read n at runtime so the VM can't pre-compute the result.
local n = tonumber(arg and arg[1]) or 35

local t0 = os.clock()
local result = fib(n)
local elapsed_ms = (os.clock() - t0) * 1000

print(result)
io.stderr:write(string.format("(%.1fms)\n", elapsed_ms))
