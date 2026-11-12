"""Verify rendered WKWebView content as well as the final bundle's native API."""
import argparse,json,os,secrets,socket,subprocess,tempfile,time
from pathlib import Path

parser=argparse.ArgumentParser()
parser.add_argument('app',type=Path)
parser.add_argument('--out',type=Path,required=True)
args=parser.parse_args();args.app=args.app.resolve();args.out=args.out.resolve();args.out.mkdir(parents=True,exist_ok=True)
identifier=json.loads((Path(__file__).parent.parent/'desktop/tauri.conf.json').read_text())['identifier']
desktop=args.app/'Contents/MacOS/praxis-relay-desktop';headless=args.app/'Contents/MacOS/praxis-relay-headless'
ocr=args.out/'recognize'
subprocess.run(['swiftc',str(Path(__file__).with_name('ocr.swift')),'-o',str(ocr)],check=True)
quitter=args.out/'quit-application'
subprocess.run(['swiftc',str(Path(__file__).with_name('quit_mac.swift')),'-o',str(quitter)],check=True)
def run(*cmd,**kwargs):return subprocess.check_output(list(map(str,cmd)),encoding='utf-8',**kwargs).strip()
def wait_for(function,timeout=40):
 until=time.monotonic()+timeout
 while time.monotonic()<until:
  value=function()
  if value:return value
  time.sleep(.3)
 raise AssertionError('Native rendered UI did not become ready')
checks=[]
def check(condition,name):
 assert condition,name
 checks.append(name)
with tempfile.TemporaryDirectory(prefix='relay-rendered-') as temporary:
 root=Path(temporary);auth=root/'data';auth.mkdir()
 with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
 env=dict(os.environ,RELAY_AUTH_DIR=str(auth),RELAY_PORT=str(port),RELAY_NO_BROWSER='1',RELAY_CODEX_VERSION='0.163.0',RELAY_MODEL_DISCOVERY='off',
          HTTPS_PROXY='http://127.0.0.1:1',HTTP_PROXY='http://127.0.0.1:1',NO_PROXY='127.0.0.1,localhost')
 env.pop('RELAY_API_KEY',None)
 def cli(*cmd):return json.loads(run(headless,*cmd,env=env,timeout=20))
 patch=root/'settings.json';patch.write_text(json.dumps({'appearance':{'theme':'light'}}));cli('configure',patch)
 with (args.out/'native-process.log').open('wb') as log:
  process=subprocess.Popen([str(desktop),'--start-api'],env=env,stdout=log,stderr=log)
  try:
   wait_for(lambda:(auth/'control.json').exists())
   status=cli('status');key=status['api_key'];check(status['running'],'final app serves the local API')
   def rendered(name,label=None):
    target=args.out/name
    run('screencapture','-x',target)
    text=json.loads(run(ocr,target));combined=''.join(text).replace(' ','')
    (args.out/(name+'.ocr.json')).write_text(json.dumps(text,ensure_ascii=False,indent=2))
    return '127.0.0.1' in combined and str(port) in combined and (label is None or label in combined)
   wait_for(lambda:rendered('native-light.png'))
   check(True,'light WKWebView renders its actual API address, confirmed by native OCR')
   duplicate=subprocess.run([str(desktop)],env=env,stdout=log,stderr=log,timeout=20)
   check(duplicate.returncode==0 and process.poll() is None,'second launch preserves the original native instance')
   # Avoid the most ambiguous small glyphs: 0/O, 1/I and 7/slash. Alternate
   # Vision candidates still have to contain this exact random fixture nonce.
   marker = ''.join(secrets.choice('2345689') for _ in range(6))
   label = 'QA ' + marker
   (args.out/'fixture.json').write_text(
       json.dumps({'port':port,'label':label,'ocr_marker':marker}), encoding='utf8')
   patch.write_text(json.dumps({'appearance':{'theme':'dark'},'labels':{'primary':label}}))
   cli('configure',patch)
   check(cli('status')['settings']['labels']['primary']==label,'configured account label matches the OCR fixture nonce')
   wait_for(lambda:rendered('native-dark.png',marker))
   check(True,'dark theme and changed account label reach the rendered WKWebView')
   run(quitter,process.pid)
   check(process.wait(timeout=15)==0,'native Quit exits the final bundle cleanly')
   check(not (auth/'control.json').exists(),'native Quit removes the running capability')
   check((auth/'api-key.txt').read_text().strip()==key,'native Quit preserves the persistent client key')
  finally:
   if process.poll() is None:process.terminate();process.wait(timeout=15)
proof=dict(passed=True,checks=checks,fixture_label=label,ocr_marker=marker)
(args.out/'native-acceptance.json').write_text(json.dumps(proof,indent=2)+'\n')
print(json.dumps(proof,indent=2))
