import sqlite3
import os
import sys
from datetime import datetime

sys.stdout.reconfigure(encoding='utf-8')

db_path = os.path.join(os.path.expanduser('~'), '.codex', 'logs_2.sqlite')

conn = sqlite3.connect(db_path)
cursor = conn.cursor()

# Timestamp for 2026-06-03 10:20:00
# 10:20:00 local time = 02:20:00 UTC. Unix timestamp is 1780453200.
start_ts = 1780453200

print(f"Filtering logs from logs_2.sqlite since: {start_ts} (2026-06-03 10:20:00)")

cursor.execute("""
    SELECT id, ts, level, target, feedback_log_body 
    FROM logs 
    WHERE ts >= ?
    ORDER BY id ASC;
""", (start_ts,))

rows = cursor.fetchall()
print(f"Total log rows since 10:20: {len(rows)}")

for row in rows:
    dt = datetime.fromtimestamp(row[1]).strftime('%Y-%m-%d %H:%M:%S')
    body = row[4]
    if len(body) > 300:
        body = body[:200] + " ... [TRUNCATED] ... " + body[-100:]
    print(f"[{dt}] [{row[2]}] {row[3]}: {body}")
