import os

target_dir = r"e:\项目\codex-workspace-mcp"
exclude_dirs = {'.git', '.gemini', '.claude', '.codex-workspace-mcp', 'target', 'node_modules'}
exclude_files = {'Cargo.lock'}

print(f"{'File Path':<60} | {'Lines':<10}")
print("-" * 75)

for root, dirs, files in os.walk(target_dir):
    dirs[:] = [d for d in dirs if d not in exclude_dirs]
    for file in files:
        if file in exclude_files:
            continue
        file_path = os.path.join(root, file)
        try:
            with open(file_path, 'r', encoding='utf-8', errors='ignore') as f:
                lines = sum(1 for _ in f)
            if lines > 1000:
                rel_path = os.path.relpath(file_path, target_dir)
                print(f"{rel_path:<60} | {lines:<10}")
        except Exception as e:
            pass
