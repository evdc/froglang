-- Order-pipeline benchmark — runs on both Lua 5.x and LuaJIT.
-- Mirrors benches/orders.frog exactly; see that file for what it measures.
--
--   lua orders.lua [items] [rounds]   /   luajit orders.lua [items] [rounds]

local FOOD, BOOK, ELECTRONICS, TOY = 0, 1, 2, 3
local NO_DISCOUNT, PERCENT, FLAT, BULK_OVER = 0, 1, 2, 3

-- LuaJIT (5.1 semantics) has no integer type and `/` is float division, so
-- integer division is spelled with floor.  Every operand here is
-- non-negative, so this matches the truncating division the other
-- implementations do.
local floor = math.floor
local function idiv(a, b) return floor(a / b) end

local function hash(i) return (i * 2654435761 + 1013904223) % 2147483647 end

local function category_of(h) return h % 4 end

local function discount_for(it)
    local c = it.category
    if c == FOOD then
        if it.qty >= 6 then return BULK_OVER, 6, 5 else return NO_DISCOUNT, 0, 0 end
    elseif c == BOOK then
        return PERCENT, 10, 0
    elseif c == ELECTRONICS then
        if it.unit_price > 3000 then return FLAT, 250, 0 else return PERCENT, 3, 0 end
    else
        return BULK_OVER, 3, 15
    end
end

local function apply(kind, a, pct, gross, qty)
    if kind == NO_DISCOUNT then
        return gross
    elseif kind == PERCENT then
        return gross - idiv(gross * a, 100)
    elseif kind == FLAT then
        if gross > a then return gross - a else return 0 end
    else
        if qty >= a then return gross - idiv(gross * pct, 100) else return gross end
    end
end

local items_n = tonumber(arg and arg[1]) or 2000
local rounds = tonumber(arg and arg[2]) or 2000

local t0 = os.clock()

local items = {}
for i = 0, items_n - 1 do
    items[i + 1] = {
        sku = i,
        category = category_of(hash(i)),
        qty = 1 + hash(i + 7) % 9,
        unit_price = 100 + hash(i + 13) % 5000,
    }
end

local total = 0
for round = 0, rounds - 1 do
    local batch, n = {}, 0
    for k = 1, items_n do
        local it = items[k]
        if (it.sku + round) % 3 ~= 0 then
            n = n + 1
            batch[n] = it
        end
    end
    for k = 1, n do
        local it = batch[k]
        local kind, a, pct = discount_for(it)
        total = total + apply(kind, a, pct, it.qty * it.unit_price, it.qty)
    end
end

local elapsed_ms = (os.clock() - t0) * 1000
print(string.format("%.0f", total))
io.stderr:write(string.format("(%.1fms)\n", elapsed_ms))
