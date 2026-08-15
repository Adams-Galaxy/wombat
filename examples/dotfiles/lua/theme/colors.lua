local function alpha_byte(amount)
    assert(type(amount) == "number" and amount >= 0 and amount <= 1,
        "alpha must be a number between 0 and 1")
    return string.format("%02x", math.floor(amount * 255 + 0.5))
end

return {
    alpha = function(color, amount, options)
        assert(type(color) == "string" and color:match("^#%x%x%x%x%x%x$"),
            "color must use #RRGGBB")
        return color .. alpha_byte(amount) .. (options.suffix or "")
    end,
}
