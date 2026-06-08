import sqlite3
import os
import sys

sys.stdout.reconfigure(encoding='utf-8')

db_path = r"e:\项目\codex-workspace-mcp\target\release\.codex-workspace-mcp\codex_state.db"

if not os.path.exists(db_path):
    print("Database file not found at", db_path)
    sys.exit(1)

conn = sqlite3.connect(db_path)
cursor = conn.cursor()

# Query logs that contain "VISION" or "Image"
cursor.execute("""
    SELECT id, time_str, action, role, message, detail 
    FROM api_logs 
    WHERE (action LIKE '%VISION%' OR message LIKE '%VISION%' OR message LIKE '%Image%') 
      AND time_str >= '2026-06-05 09:50:00' 
    ORDER BY id ASC
""")

rows = cursor.fetchall()
print(f"Total VISION-related logs: {len(rows)}")
print("=" * 80)
for row in rows:
    detail_str = f" | Detail: {row[5]}" if row[5] else ""
    print(f"[{row[1]}] [{row[2]}] [{row[3]}]: {row[4]}{detail_str}")

conn.close()
