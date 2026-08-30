-- Conway's Game of Life — the Lua sibling of benches/life.frog.
-- Must print the same checksum.  Runs on both Lua 5.x and LuaJIT.
-- Usage: lua life.lua <size> <generations>  /  luajit life.lua ...
--
-- Lua tables are 1-based; the grid is indexed 1..size here while the
-- froglang/Rust/Go/Python versions index 0..size-1.  The seed hash still
-- uses 0-based coordinates so every implementation starts from the same
-- grid.

-- LuaJIT (5.1 semantics) has no integer type and no `//` operator, so
-- integer division is spelled with floor.  Every operand here is
-- non-negative, so this matches the truncating division the other
-- implementations do.
local floor = math.floor

local function modn(x, n)
    return x - floor(x / n) * n
end

local function hash(i)
    return modn(i * 2654435761 + 1013904223, 2147483647)
end

local function seed(size)
    local rows = {}
    for y = 1, size do
        local row = {}
        for x = 1, size do
            row[x] = modn(hash((y - 1) * size + (x - 1)), 3) == 0 and 1 or 0
        end
        rows[y] = row
    end
    return rows
end

local function cell_at(rows, y, x)
    local size = #rows
    if y >= 1 and y <= size and x >= 1 and x <= size then
        return rows[y][x]
    end
    return 0
end

local function neighbours(rows, y, x)
    local n = 0
    for dy = -1, 1 do
        for dx = -1, 1 do
            if not (dx == 0 and dy == 0) then
                n = n + cell_at(rows, y + dy, x + dx)
            end
        end
    end
    return n
end

local function next_cell(rows, y, x)
    local alive = rows[y][x]
    local n = neighbours(rows, y, x)
    if alive == 1 then
        return (n == 2 or n == 3) and 1 or 0
    end
    return n == 3 and 1 or 0
end

local function step(rows)
    local size = #rows
    local out = {}
    for y = 1, size do
        local row = {}
        for x = 1, size do
            row[x] = next_cell(rows, y, x)
        end
        out[y] = row
    end
    return out
end

local function population(rows)
    local total = 0
    for y = 1, #rows do
        local row = rows[y]
        for x = 1, #row do
            total = total + row[x]
        end
    end
    return total
end

local size = tonumber(arg[1]) or 48
local generations = tonumber(arg[2]) or 40

local start = os.clock()
local grid = seed(size)
local checksum = 0
for _ = 1, generations do
    checksum = checksum + population(grid)
    grid = step(grid)
end
local elapsed = (os.clock() - start) * 1000

print(string.format("%.0f", checksum))
io.stderr:write(string.format("%.1fms\n", elapsed))
