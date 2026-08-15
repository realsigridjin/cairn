#!/usr/bin/env python3
from pathlib import Path
import hashlib, json, re, stat, sys, tomllib
root=Path(__file__).resolve().parents[1]; errors=[]
def need(rel):
 p=root/rel
 if not p.is_file() or p.stat().st_size==0: errors.append(f'missing/empty: {rel}')
 return p
required=['Cargo.toml','Cargo.uqa.toml','README.md','DEEPSEEK_HARNESS.md','REVIEW.md','VALIDATION.md','SESSION_CONTINUITY.md','src/server.rs','src/model.rs','src/runtime.rs','src/bin/cairn/cli.rs','integrations/deepseek-harness/package.json','integrations/deepseek-harness/cordis.patch.yml','integrations/deepseek-harness/src/index.ts','integrations/deepseek-harness/src/client.ts','integrations/deepseek-harness/src/protocol.ts','integrations/deepseek-harness/lib/index.js','integrations/deepseek-harness/test/plugin.test.mjs','integrations/deepseek-harness/bin/cairn-dsh-doctor.mjs','scripts/install_dsh_bundle.sh','web/package.json','web/src/server.ts','web/src/session-packet.ts','web/src/history-store.ts','web/test/session-packet.test.ts','web/test/history-durable.test.ts','dist/cairn-uqa-dsh-1.0.0.tgz','dist/cairn-uqa-dsh-1.0.0.tgz.sha256']
for rel in required: need(rel)
try:
 with (root/'Cargo.toml').open('rb') as f: cargo=tomllib.load(f)
 if cargo['package']['version']!='1.0.0': errors.append('Cargo version must be 1.0.0')
 if cargo.get('features',{}).get('default')!=[]: errors.append('UQA must remain opt-in')
 if 'uqa-engine' in (root/'Cargo.toml').read_text() or 'uqa-core' in (root/'Cargo.toml').read_text(): errors.append('standalone Cargo.toml must not resolve UQA path dependencies')
 with (root/'Cargo.uqa.toml').open('rb') as f: uqa_cargo=tomllib.load(f)
 if uqa_cargo['package']['version']!='1.0.0': errors.append('Cargo.uqa version must be 1.0.0')
 if uqa_cargo.get('features',{}).get('uqa')!=['dep:uqa-engine','dep:uqa-core']: errors.append('Cargo.uqa must expose the UQA feature')
except Exception as e: errors.append(f'Cargo parse: {e}')
try:
 pkg=json.loads((root/'integrations/deepseek-harness/package.json').read_text())
 if pkg.get('version')!='1.0.0' or pkg.get('dsh',{}).get('bundle',{}).get('patch')!='./cordis.patch.yml': errors.append('invalid DSH package metadata')
 if pkg.get('engines',{}).get('node')!='^22.19.0 || >=24.0.0': errors.append('wrong DSH Node floor')
except Exception as e: errors.append(f'plugin package parse: {e}')
for rel in ['examples/scoring.example.json']:
 try: json.loads((root/rel).read_text())
 except Exception as e: errors.append(f'{rel}: {e}')
for rel in ['examples/chunks.jsonl','examples/chunks-text-only.jsonl']:
 try:
  for line in (root/rel).read_text().splitlines():
   if line.strip(): json.loads(line)
 except Exception as e: errors.append(f'{rel}: {e}')
patch=(root/'integrations/deepseek-harness/cordis.patch.yml').read_text()
for bad in ['!!js','process.env.CAIRN_DSH_BASE_URL','process.env.CAIRN_DSH_TOKEN_ENV']:
 if bad in patch: errors.append(f'unsafe bundle expression: {bad}')
search_roots=[root/'src',root/'tests',root/'scripts',root/'integrations',root/'web',root/'examples',root/'cloudflare']
marker_corpus='\n'.join(
 p.read_text(errors='ignore')
 for base in search_roots
 for p in base.rglob('*')
 if p.is_file() and 'node_modules' not in p.parts and '__pycache__' not in p.parts
)
for marker in ['ctx.tools.register(defineTool','exec.signal','exec.callId','BEGIN_UNTRUSTED_CAIRN_EVIDENCE','fixedFiltersJson','CAIRN_HTTP_API_VERSION','WWW_AUTHENTICATE','allowed_scope','/v1/{tenant}/kb/{kb}/head','embedding_provider']:
 if marker not in marker_corpus: errors.append(f'missing marker: {marker}')
for p in [*root.joinpath('src').rglob('*.rs'),*root.joinpath('tests').rglob('*.rs')]:
 t=p.read_text()
 for pattern,label in [(r'\.unwrap\(\)','unwrap'),(r'\.expect\(','expect'),(r'\bpanic!\s*\(','panic'),(r'\btodo!\s*\(','todo'),(r'\bunimplemented!\s*\(','unimplemented'),(r'\bunsafe\s*\{','unsafe')]:
  if re.search(pattern,t): errors.append(f'{label}: {p.relative_to(root)}')
 if t.count('{')!=t.count('}'): errors.append(f'brace imbalance: {p.relative_to(root)}')
for rel in ['scripts/install_dsh_bundle.sh','integrations/deepseek-harness/bin/cairn-dsh-doctor.mjs']:
 p=root/rel
 if p.exists() and not (p.stat().st_mode&stat.S_IXUSR): errors.append(f'not executable: {rel}')
try:
 line=(root/'dist/cairn-uqa-dsh-1.0.0.tgz.sha256').read_text().split()[0]
 if hashlib.sha256((root/'dist/cairn-uqa-dsh-1.0.0.tgz').read_bytes()).hexdigest()!=line: errors.append('plugin tgz digest mismatch')
except Exception as e: errors.append(f'plugin digest: {e}')
if errors:
 print('STATIC VALIDATION FAILED'); print('\n'.join('- '+x for x in errors)); sys.exit(1)
print('STATIC VALIDATION PASSED')
excluded={'.git','.omo','target','node_modules','__pycache__'}
print(f'files={sum(1 for p in root.rglob("*") if p.is_file() and not any(part in excluded for part in p.parts))}')
