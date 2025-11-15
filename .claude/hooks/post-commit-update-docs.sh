#!/bin/bash
# Post-commit hook to prompt Claude to update DEPENDENCIES.md
# This hook triggers after any 'git commit' command via Claude Code

set -e

# Read JSON input from stdin (more reliable than environment variables)
input=$(cat)

# Extract the bash command that was executed
# Uses jq to parse JSON safely
command=$(echo "$input" | jq -r '.tool_input.command // empty' 2>/dev/null || echo "")

# Check if this was a git commit command
if [[ "$command" =~ ^git[[:space:]]+commit ]]; then
    echo "Detected git commit command, triggering documentation update..." >&2
    echo "" >&2
    echo "A git commit was just made. Please review the commit changes and update DEPENDENCIES.md to reflect any:" >&2
    echo "  - Architectural changes" >&2
    echo "  - New or modified dependencies" >&2
    echo "  - Implementation changes that affect the documentation" >&2
    echo "  - Module structure updates" >&2
    echo "  - Data flow modifications" >&2
    echo "" >&2
    echo "Review the latest commit with 'git show HEAD' and update the relevant sections of DEPENDENCIES.md accordingly." >&2

    # Exit code 2 triggers Claude to automatically respond to this message
    exit 2
fi

# Not a git commit command, continue silently
exit 0
