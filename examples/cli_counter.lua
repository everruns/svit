function main(input)
    local by = input and input.by or 1
    memory.count = (memory.count or 0) + by
    return { count = memory.count }
end
