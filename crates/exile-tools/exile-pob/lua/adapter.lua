-- JSON-lines stdio adapter over a STOCK Path of Building checkout
-- (Path of Exile 1 or 2 — the engine is chosen by the working directory).
--
-- This file is embedded into exile-pob at build time and materialized
-- to a temp file at spawn; the tool runs it as `luajit <adapter>` with
-- cwd = <checkout>/src. For a manual run, invoke it the same way:
--   cd vendor/pob/<game>/src && luajit <path to this file>
--
-- Protocol: one JSON object per stdin line -> one JSON object per stdout
-- line. Engine noise goes to stderr; after boot the adapter prints a
-- ready banner on stdout, so hosts read stdout lines and ignore
-- everything before the first parseable {"ready":true,...}.
--
-- Requests:  {"id":1,"cmd":"version"}
--            {"id":2,"cmd":"new"}
--            {"id":3,"cmd":"loadXML","xml":"<PathOfBuilding>...","name":"x"}
--            {"id":4,"cmd":"loadCode","code":"<base64url build code>"}
--            {"id":5,"cmd":"makeCode"}
--            {"id":6,"cmd":"stats","keys":["Life","TotalDPS"]}
--            {"id":7,"cmd":"quit"}
-- Responses: {"id":N,"ok":true,"result":...} | {"id":N,"ok":false,"error":"..."}
-- Request ids must be >= 1; a line that fails to parse as JSON is
-- answered with id 0 (unattributable) plus the parser's error detail.
--
-- Both engines stub the SimpleGraphic host functions Inflate/Deflate to
-- return "" in headless mode, which silently breaks build-code
-- import/export. This adapter restores them with a LuaJIT FFI binding to
-- zlib (the checkout's own runtime/zlib1.dll on Windows, the system
-- libz elsewhere), so import codes work fully in-engine.

-- Engine boot prints via ConPrintf, which calls the *global* print at
-- call time — reroute it to stderr before boot so stdout stays a clean
-- protocol channel.
local emit = io.stdout
print = function(...)
    local parts = {}
    for index = 1, select("#", ...) do
        parts[index] = tostring(select(index, ...))
    end
    io.stderr:write(table.concat(parts, "\t"), "\n")
end

-- The exact lpath/cpath the engines' own busted CI uses, plus the
-- checkout's runtime dir for native modules (lua-utf8.dll). Set here so
-- hosts need no LUA_PATH/LUA_CPATH environment.
package.path = "../runtime/lua/?.lua;../runtime/lua/?/init.lua;" .. package.path
package.cpath = "../runtime/?.dll;../runtime/?.so;" .. package.cpath

local probe = io.open("HeadlessWrapper.lua", "r")
if not probe then
    io.stderr:write("adapter must run with cwd = <PoB checkout>/src\n")
    os.exit(1)
end
probe:close()

local json = require("dkjson")

dofile("HeadlessWrapper.lua")

if type(newBuild) ~= "function" or not build then
    -- The wrapper prints mainObject.promptMsg itself on boot failure.
    io.stderr:write("engine boot failed: HeadlessWrapper did not expose the build API\n")
    os.exit(1)
end

-- Restore Inflate/Deflate (stubbed to "" headless in both engines) via
-- FFI zlib. Failure is non-fatal: loadCode/makeCode report it, XML
-- commands keep working.
local zlib_error
do
    local ok, err = pcall(function()
        local ffi = require("ffi")
        ffi.cdef([[
            int uncompress(uint8_t *dest, unsigned long *destLen,
                           const char *source, unsigned long sourceLen);
            int compress2(uint8_t *dest, unsigned long *destLen,
                          const char *source, unsigned long sourceLen, int level);
            unsigned long compressBound(unsigned long sourceLen);
        ]])
        local zlib
        for _, candidate in ipairs({ "../runtime/zlib1.dll", "z", "zlib1" }) do
            local loaded, lib = pcall(ffi.load, candidate)
            if loaded then
                zlib = lib
                break
            end
        end
        assert(zlib, "no zlib library found (tried runtime/zlib1.dll, z, zlib1)")

        Inflate = function(data)
            local capacity = 8 * #data + 1024
            for _ = 1, 8 do
                local buffer = ffi.new("uint8_t[?]", capacity)
                local length = ffi.new("unsigned long[1]", capacity)
                local rc = zlib.uncompress(buffer, length, data, #data)
                if rc == 0 then
                    return ffi.string(buffer, length[0])
                elseif rc ~= -5 then -- Z_BUF_ERROR: grow and retry
                    return nil, "zlib uncompress failed: " .. rc
                end
                capacity = capacity * 4
            end
            return nil, "zlib inflate: output larger than retry cap"
        end
        Deflate = function(data)
            local capacity = tonumber(zlib.compressBound(#data))
            local buffer = ffi.new("uint8_t[?]", capacity)
            local length = ffi.new("unsigned long[1]", capacity)
            local rc = zlib.compress2(buffer, length, data, #data, 9)
            if rc ~= 0 then
                return nil, "zlib compress failed: " .. rc
            end
            return ffi.string(buffer, length[0])
        end
    end)
    if not ok then
        zlib_error = tostring(err)
        io.stderr:write("zlib unavailable, build codes disabled: ", zlib_error, "\n")
    end
end

local DEFAULT_STATS = {
    "Life", "Mana", "EnergyShield", "Ward", "Armour", "Evasion", "TotalEHP",
    "FireResist", "ColdResist", "LightningResist", "ChaosResist",
    "TotalDPS", "CombinedDPS", "FullDPS", "AverageDamage", "Speed",
    "CritChance", "CritMultiplier",
}

-- JSON has no inf/nan; PoB uses math.huge (e.g. a max hit nothing can
-- reach), so encode those as strings.
local function sanitize(value)
    if value ~= value then
        return "nan"
    elseif value == math.huge then
        return "inf"
    elseif value == -math.huge then
        return "-inf"
    end
    return value
end

local handlers = {}

function handlers.version()
    return { engine = tostring(launch and launch.versionNumber or "unknown") }
end

function handlers.new()
    newBuild()
    return { loaded = true }
end

function handlers.loadXML(params)
    assert(type(params.xml) == "string" and #params.xml > 0, "`xml` (string) required")
    loadBuildFromXML(params.xml, params.name or "adapter")
    return { loaded = true }
end

function handlers.loadCode(params)
    assert(type(params.code) == "string" and #params.code > 0, "`code` (string) required")
    assert(not zlib_error, "build codes unavailable: " .. tostring(zlib_error))
    local compressed = common.base64.decode(params.code:gsub("-", "+"):gsub("_", "/"))
    assert(compressed, "invalid base64 in build code")
    local xml, err = Inflate(compressed)
    assert(xml and #xml > 0, "build code did not inflate: " .. tostring(err))
    loadBuildFromXML(xml, params.name or "adapter")
    return { loaded = true }
end

function handlers.makeCode()
    assert(not zlib_error, "build codes unavailable: " .. tostring(zlib_error))
    local xml = build:SaveDB("code")
    assert(type(xml) == "string" and #xml > 0, "engine returned no build XML")
    local compressed, err = Deflate(xml)
    assert(compressed, "deflate failed: " .. tostring(err))
    local code = common.base64.encode(compressed):gsub("+", "-"):gsub("/", "_")
    return { code = code }
end

function handlers.stats(params)
    assert(params.keys == nil or type(params.keys) == "table",
        "`keys` must be an array of stat names")
    if build.calcsTab.BuildOutput then
        build.calcsTab:BuildOutput()
    end
    local out = build.calcsTab.mainOutput or {}
    local result = {}
    for _, key in ipairs(params.keys or DEFAULT_STATS) do
        local value = type(key) == "string" and out[key] or nil
        if type(value) == "number" then
            result[key] = sanitize(value)
        elseif type(value) == "string" or type(value) == "boolean" then
            result[key] = value
        end
    end
    return result
end

local function respond(response)
    emit:write(json.encode(response), "\n")
    emit:flush()
end

respond({ ready = true, engine = tostring(launch and launch.versionNumber or "unknown") })

for line in io.stdin:lines() do
    line = line:gsub("\r$", "") -- tolerate CRLF hosts
    if line ~= "" then
        local request, _, decode_error = json.decode(line)
        if type(request) ~= "table" then
            respond({
                id = 0,
                ok = false,
                error = "unparseable request line: " .. tostring(decode_error or "not a JSON object"),
            })
        elseif request.cmd == "quit" then
            respond({ id = request.id, ok = true, result = { bye = true } })
            break
        else
            local handler = handlers[request.cmd]
            if not handler then
                respond({ id = request.id, ok = false, error = "unknown cmd: " .. tostring(request.cmd) })
            else
                local ok, result = pcall(handler, request)
                if ok then
                    respond({ id = request.id, ok = true, result = result })
                else
                    respond({ id = request.id, ok = false, error = tostring(result) })
                end
            end
        end
    end
end
