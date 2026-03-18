<!-- markdownlint-disable MD041 -->
You have access to the failure analysis data for a C-to-Rust translation project.

The data file is at: DATA_FILE_PLACEHOLDER

It contains fix-result.json and fix.patch for each failing test case across multiple batteries.

Write a comprehensive analysis report in markdown to: OUTPUT_PLACEHOLDER

The report should include:
1. An executive summary with overall numbers
2. Per-battery breakdown with pass/fail counts
3. Root cause analysis — group the fixes by what type of bug they fixed (e.g. floating point precision, integer overflow, missing FFI exports, hash buffer handling, etc.) with counts and examples
4. Common patterns that could improve the original translation prompts
5. Cases that could not be fixed and why

Read the data file and the patches carefully. Base your analysis on what the patches actually changed, not assumptions.
