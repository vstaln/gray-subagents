"""Manual local PTY smoke check (requires pyte and the isolated host build)."""
import os,pathlib,tempfile,subprocess,json,pty,select,time,fcntl,termios,struct,pyte,codecs
host='<gray-checkout-with-host-bridge>/target/debug/gray'
plugin='~/grayplugins/gray-subagents/target/debug/gray-subagents'
with tempfile.TemporaryDirectory(prefix='gray-widget-pty-') as tmp:
 env=dict(os.environ,GRAY_HOME=tmp,GRAY_PLUGIN_PATH=plugin,TERM='xterm-256color',GRAY_MODEL='test/model',GRAY_BASE_URL='http://127.0.0.1:9/v1',GRAY_CONTEXT_WINDOW='32000')
 out=subprocess.run([host,'install','plugin','subagents'],env=env,capture_output=True,text=True,timeout=10)
 assert out.returncode==0,out.stderr
 (pathlib.Path(tmp)/'plugins/widgets.json').write_text(json.dumps({'name':'subagents','argv':[plugin,'widget','--demo']}))
 master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,0,0))
 p=subprocess.Popen([host],stdin=slave,stdout=slave,stderr=slave,env=env,cwd=tmp,start_new_session=True);os.close(slave)
 screen=pyte.Screen(100,30);stream=pyte.Stream(screen);decoder=codecs.getincrementaldecoder('utf-8')('replace');raw=b''
 def drain(seconds):
  global raw
  deadline=time.monotonic()+seconds
  while time.monotonic()<deadline:
   if select.select([master],[],[],.1)[0]:
    try: chunk=os.read(master,65536)
    except OSError:break
    if not chunk:break
    raw+=chunk;stream.feed(decoder.decode(chunk))
    if b'\x1b[6n' in chunk:os.write(master,f'\x1b[{screen.cursor.y+1};{screen.cursor.x+1}R'.encode())
 try:
  drain(5)
  os.write(master,b'draft stays here');drain(1)
  text='\n'.join(screen.display)
  pathlib.Path('/tmp/gray-subagents-terminal.txt').write_text(text+'\n')
  pathlib.Path('/tmp/gray-subagents-terminal.ansi').write_bytes(raw)
  print(text)
  assert 'Scout' in text and 'Reviewer' in text and '⬢ Agents' in text
  assert 'draft stays here' in text
  assert text.index('⬢ Agents')<text.index('draft stays here')
  print('PASS real PTY: plugin widget is above the editable draft')
  os.write(master,b'\x15/subagents settings\r');drain(2)
  text='\n'.join(screen.display)
  assert 'max_running' in text,'slash settings did not render'
  print('PASS /subagents settings renders without a model call')
 finally:
  p.terminate()
  try:p.wait(timeout=5)
  except subprocess.TimeoutExpired:p.kill();p.wait()
  os.close(master)
