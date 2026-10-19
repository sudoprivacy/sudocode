import importlib.util
import json
import os
from pathlib import Path
import statistics
import subprocess

ROOT=Path.cwd()
OUT=Path(os.environ['RUNNER_TEMP'])/'ascii-cell-pilot'
OUT.mkdir()
spec=importlib.util.spec_from_file_location('benchmark',ROOT/'e2e/terminal-resize/benchmark.py')
benchmark=importlib.util.module_from_spec(spec);spec.loader.exec_module(benchmark)
policy_path=ROOT/'e2e/terminal-resize/performance-policy.json'
policy=json.loads(policy_path.read_text())
messages=[json.loads(line) for line in Path(os.environ['RUNNER_TEMP'],'driver.jsonl').read_text().splitlines()]
drivers=[x['executable'] for x in messages if x.get('reason')=='compiler-artifact' and x.get('executable') and x['target']['name']=='pty_render_performance']
assert len(drivers)==1,drivers
results=[]
for round_number in range(4):
 for case in ['cap','long']:
  sides=['fixed','prototype'] if round_number%2==0 else ['prototype','fixed']
  for side in sides:
   target=OUT/f'{round_number}-{case}-{side}';target.mkdir()
   report=target/'sample.json'
   binary=Path(os.environ['RUNNER_TEMP'])/('scode-fixed' if side=='fixed' else 'scode-prototype')
   env=dict(os.environ,SCODE_TEST_BACKEND='mock',SCODE_TEST_BIN=str(binary),SUDOCODE_EXPERIMENT_PROSE_PREVIEW='0',SCODE_RENDER_CASE=case,SCODE_RENDER_POLICY=str(policy_path),SCODE_RENDER_REPORT=str(report),SCODE_TERMINAL_LOG_DIR=str(target))
   env.pop('NO_COLOR',None)
   print(f'RUN {round_number} {case} {side}',flush=True)
   with (target/'driver.log').open('w') as output:
    subprocess.run([drivers[0],'--ignored','--exact','render_performance_workload','--nocapture'],cwd=ROOT,env=env,stdout=output,stderr=subprocess.STDOUT,check=True,timeout=150)
   sample=json.loads(report.read_text());benchmark.validate(sample,case,policy)
   row={'round':round_number,'warmup':round_number==0,'case':case,'side':side,'metrics':sample['metrics']}
   results.append(row)
   (OUT/'results.json').write_text(json.dumps({'scope':'diagnostic only; three paired rounds after warmup; original workloads; not full calibrated gate','results':results},indent=2)+'\n')
   print(json.dumps(row),flush=True)
summary=[]
for case in ['cap','long']:
 for side in ['fixed','prototype']:
  rows=[x for x in results if not x['warmup'] and x['case']==case and x['side']==side]
  summary.append({'case':case,'side':side,'metrics':{key:statistics.median(x['metrics'][key] for x in rows) for key in ['rss_after_mib','rss_peak_mib','cpu_ms','input_stream_p95_ms']}})
(OUT/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary),flush=True)
