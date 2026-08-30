-- Word-pipeline benchmark — the Lua sibling of benches/words.frog.
-- Must print the same checksum.  Runs on both Lua 5.x and LuaJIT.
-- Usage: lua words.lua <words_per_doc> <rounds>  /  luajit words.lua ...

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

local VOCAB = {"frog", "frogs", "toad", "newt", "salamander", "axolotl", "tadpole", "pond"}

-- VOCAB is 1-based here; the other implementations index 0..7.
local function word_for(i)
    return VOCAB[modn(hash(i), 8) + 1]
end

local function build_doc(words_per_doc, round)
    local base = round * words_per_doc
    local ws = {}
    for i = 0, words_per_doc - 1 do
        ws[i + 1] = word_for(base + i)
    end
    return table.concat(ws, " ")
end

local function score(w)
    local base = #w
    local bonus = (w:sub(1, 4) == "frog") and 10 or 0
    local exact = (w == "frog") and 5 or 0
    return base + bonus + exact
end

local words_per_doc = tonumber(arg[1]) or 400
local rounds = tonumber(arg[2]) or 800

local start = os.clock()
local checksum = 0
for round = 0, rounds - 1 do
    local doc = build_doc(words_per_doc, round)
    local total = 0
    -- `gmatch` over non-space runs is the idiomatic split; the document has
    -- no empty fields, so this matches `split(" ")` in the others.
    for w in doc:gmatch("[^ ]+") do
        total = total + score(w)
    end
    local shouted = doc:upper()
    local found = shouted:find("SALAMANDER", 1, true) and 1 or 0
    checksum = checksum + total + #doc + found
end
local elapsed = (os.clock() - start) * 1000

print(string.format("%.0f", checksum))
io.stderr:write(string.format("%.1fms\n", elapsed))
