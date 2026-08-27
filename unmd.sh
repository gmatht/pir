#!/bin/bash

# unmd.sh
# Usage: ./unmd.sh < input.md
#    or: ./unmd.sh input.md

CURRENT_FILE=""
CONTENT=""
IN_BLOCK=false

# Read from file argument or stdin
if [ -n "$1" ]; then
    exec < "$1"
fi

while IFS= read -r line || [ -n "$line" ]; do
    # 1. Detect filename header (e.g., "### src/main.rs")
    if [[ "$line" =~ ^###\ (.+)$ ]]; then
        CURRENT_FILE="${BASH_REMATCH[1]}"
        # Trim whitespace
        CURRENT_FILE=$(echo "$CURRENT_FILE" | xargs)
        continue
    fi

    # 2. Detect code fence (```lang or ```)
    if [[ "$line" =~ ^\`\`\` ]]; then
        if [ "$IN_BLOCK" = true ]; then
            # End of code block -> write to file
            IN_BLOCK=false
            if [ -n "$CURRENT_FILE" ] && [ -n "$CONTENT" ]; then
                # Create directory if needed
                mkdir -p "$(dirname "$CURRENT_FILE")"
                # Write content. 
                # printf '%s\n' is safer than echo to handle edge cases like '-n' flags in code
                printf '%s\n' "$CONTENT" > "$CURRENT_FILE"
                echo "✅ Extracted: $CURRENT_FILE"
            fi
            # Reset for next block
            CURRENT_FILE=""
            CONTENT=""
        else
            # Start of code block
            IN_BLOCK=true
        fi
        continue
    fi

    # 3. If inside a code block, accumulate content
    if [ "$IN_BLOCK" = true ]; then
        if [ -z "$CONTENT" ]; then
            CONTENT="$line"
        else
            # Append with newline
            CONTENT="${CONTENT}"$'\n'"${line}"
        fi
    fi

done

# Handle truncated input (EOF reached while still inside a code block)
if [ "$IN_BLOCK" = true ] && [ -n "$CURRENT_FILE" ] && [ -n "$CONTENT" ]; then
    mkdir -p "$(dirname "$CURRENT_FILE")"
    printf '%s\n' "$CONTENT" > "$CURRENT_FILE"
    echo "⚠️  Extracted (partial/truncated): $CURRENT_FILE"
fi
