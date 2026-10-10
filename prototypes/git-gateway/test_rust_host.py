# SPDX-License-Identifier: Apache-2.0
"""Actual Rust serving path; Python supplies only explicit quiescent fixture inputs."""
import copy
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
from gateway.core import canonical, git_env
from gateway.demo import setup
from gateway.native_bundle import pack_fixture

READER = 'PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000'
SERVICE = 'PUBLIC_TEST_VECTOR_SERVICE_NOT_SECRET_000000'
def encoded(value):
    return (json.dumps(value,sort_keys=True,separators=(',',':'))+'\n').encode()
def sha(value): return hashlib.sha256(value).hexdigest()

@unittest.skipUnless(os.environ.get('GATEWAY_HOST'), 'GATEWAY_HOST native binary required')
class RustHostTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.shared = tempfile.TemporaryDirectory()
        cls.evidence = setup(Path(os.environ['GATEWAY_NATIVE']), Path(cls.shared.name)/'fixture')
        cls.bundle = pack_fixture(cls.evidence['config']['sources']['native-demo'])
    @classmethod
    def tearDownClass(cls): cls.shared.cleanup()
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        pairs=sorted((v['pin'],v['manifest']) for k,v in self.evidence['views'].items() if k in ('base','merged'))
        self.config={'schema':1,'expires_at':int(time.time())+600,'published_pins':[p for p,m in pairs],
            'reader_sha256':sha(READER.encode()),'service_sha256':sha(SERVICE.encode()),'views':[copy.deepcopy(m) for p,m in pairs],
            'descriptors':{'native-demo':sha(self.bundle)},'authorized_threads':{'native-demo':['main']}}
        self.path=self.root/'config.json';self.write_config()
        self.calls={'native':0,'catalog':0,'disclosure':0};self.fault=None;self.second_generation=False;self.deny_after=None
        owner=self
        class Bridge(BaseHTTPRequestHandler):
            def log_message(self,*args): pass
            def do_GET(self):
                kind,target=self.path.strip('/').split('/',1)
                owner.calls[kind]+=1
                if kind=='native': body=owner.bundle
                else:
                    m=owner.config['views'][owner.config['published_pins'].index(target)]
                    if kind=='catalog': body=canonical(m)
                    else:
                        proof={'schema':1,'authority':'quiescent-synthetic','allow':True,'audience':'public',
                            'reader_sha256':owner.config['reader_sha256'],'pin':target,'manifest_sha256':sha(canonical(m)),
                            'native_sha256':owner.config['descriptors'][m['source']],
                            'threads_sha256':sha(encoded(sorted(owner.config['authorized_threads'][m['source']]))),
                            'generation':'a'*64,'expires_at':min(int(time.time())+25,owner.config['expires_at'])}
                        if owner.fault=='missing': self.send_error(404);return
                        if owner.fault=='expired':proof['expires_at']=int(time.time())-1
                        if owner.fault=='wrong-reader':proof['reader_sha256']='b'*64
                        if owner.fault=='private':proof['audience']='private'
                        if owner.fault=='stale-source':proof['native_sha256']='b'*64
                        if owner.second_generation and owner.calls['disclosure']%2==0: proof['generation']='b'*64
                        if owner.deny_after is not None and owner.calls['disclosure']>owner.deny_after:proof['allow']=False
                        body=encoded(proof)
                self.send_response(200);self.send_header('Content-Type','application/octet-stream');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
        self.bridge=HTTPServer(('127.0.0.1',0),Bridge)
        self.thread=threading.Thread(target=self.bridge.serve_forever,daemon=True);self.thread.start()
        self.addCleanup(self.stop_bridge)
        with socket.socket() as s:s.bind(('127.0.0.1',0));self.port=s.getsockname()[1]
        self.log=(self.root/'native.log').open('w');self.addCleanup(self.log.close)
        self.process=subprocess.Popen([os.environ['GATEWAY_HOST'],'--local-test','--config',str(self.path),'--bind',f'127.0.0.1:{self.port}','--bridge',f'http://127.0.0.1:{self.bridge.server_port}'],env=git_env(),stdout=self.log,stderr=self.log)
        self.addCleanup(self.stop_process)
        for _ in range(100):
            if self.process.poll() is not None:self.fail('native host startup failed: '+(self.root/'native.log').read_text())
            try:
                with socket.create_connection(('127.0.0.1',self.port),timeout=.1):break
            except OSError:time.sleep(.02)
        else:self.fail('native host not ready')
    def write_config(self):
        temp=self.path.with_suffix('.new');temp.write_bytes(encoded(self.config));temp.replace(self.path)
    def stop_process(self):
        self.process.terminate()
        try:self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:self.process.kill();self.process.wait()
    def stop_bridge(self):self.bridge.shutdown();self.thread.join();self.bridge.server_close()
    def url(self,name='merged'):return f'http://127.0.0.1:{self.port}/views/'+self.evidence['views'][name]['pin']+'.git'
    def git(self,*args,ok=True):
        p=subprocess.run(['git','-c','http.extraHeader=Authorization: Bearer '+READER,'-c','http.extraHeader=X-Gateway-Service-Authorization: Bearer '+SERVICE,*args],env=git_env(),capture_output=True,timeout=30)
        self.assertEqual(p.returncode==0,ok,p.stderr.decode());return p
    def discovery(self,name='merged',reader=READER,service=SERVICE):
        c=http.client.HTTPConnection('127.0.0.1',self.port,timeout=30)
        c.request('GET','/views/'+self.evidence['views'][name]['pin']+'.git/info/refs?service=git-upload-pack',headers={'Authorization':'Bearer '+reader,'X-Gateway-Service-Authorization':'Bearer '+service})
        r=c.getresponse();body=r.read();status=r.status;c.close();return status,body
    def warm(self):self.assertEqual(self.discovery()[0],200);self.assertEqual(self.calls['native'],1)
    def test_clone_cache_hit_is_exact_and_request_private(self):
        clone=self.root/'clone';self.git('clone',self.url(),str(clone));self.git('-C',str(clone),'fsck','--strict')
        self.assertEqual(self.git('-C',str(clone),'rev-parse','HEAD').stdout.decode().strip(),self.evidence['views']['merged']['manifest']['git_oid'])
        self.assertEqual(self.calls['native'],1)
        self.git('ls-remote',self.url());self.assertEqual(self.calls['native'],1)
    def test_warm_reader_service_revocation_and_unpublish_deny(self):
        self.warm();before=dict(self.calls)
        for role in ('reader_sha256','service_sha256'):
            saved=self.config[role];self.config[role]='b'*64;self.write_config();self.assertEqual(self.discovery()[0],403);self.assertEqual(self.calls,before);self.config[role]=saved
        self.config['published_pins']=[];self.config['views']=[];self.write_config();self.assertEqual(self.discovery()[0],403);self.assertEqual(self.calls,before)
    def test_warm_missing_expired_wrong_audience_source_and_reader_proof_deny(self):
        self.warm()
        for fault in ('missing','expired','wrong-reader','private','stale-source'):
            self.fault=fault;status,body=self.discovery();self.assertEqual(status,403);self.assertNotIn(b'refs/heads/main',body);self.assertEqual(self.calls['native'],1)
    def test_generation_change_during_request_withholds_all_git_bytes(self):
        self.warm();self.second_generation=True
        status,body=self.discovery();self.assertEqual(status,403);self.assertNotIn(b'refs/heads/main',body);self.assertEqual(self.calls['native'],1)
    def test_revocation_between_discovery_and_post_withholds_git_bytes(self):
        self.warm();self.deny_after=self.calls['disclosure']
        c=http.client.HTTPConnection('127.0.0.1',self.port,timeout=10)
        c.request('POST','/views/'+self.evidence['views']['merged']['pin']+'.git/git-upload-pack',body=b'0000',headers={'Authorization':'Bearer '+READER,'X-Gateway-Service-Authorization':'Bearer '+SERVICE,'Content-Type':'application/x-git-upload-pack-request'})
        r=c.getresponse();body=r.read();self.assertEqual(r.status,403);self.assertNotIn(b'PACK',body);c.close();self.assertEqual(self.calls['native'],1)
    def test_epoch_change_invalidates_projection_cache(self):
        self.warm();m=self.config['views'][self.config['published_pins'].index(self.evidence['views']['merged']['pin'])];m['policy_epoch']=2;self.write_config()
        self.assertEqual(self.discovery()[0],200);self.assertEqual(self.calls['native'],2)
    def test_warm_cache_cannot_serve_restricted_state(self):
        self.warm();m=self.config['views'][self.config['published_pins'].index(self.evidence['views']['merged']['pin'])];m['state']=self.evidence['states']['restricted'];self.write_config()
        status,body=self.discovery();self.assertEqual(status,403);self.assertNotIn(b'refs/heads/main',body);self.assertEqual(self.calls['native'],2)
    def test_wrong_credentials_deny_before_any_bridge_read(self):
        for reader,service in [(READER+'x',SERVICE),(READER,SERVICE+'x')]:self.assertEqual(self.discovery(reader=reader,service=service)[0],403)
        self.assertEqual(sum(self.calls.values()),0)
    def test_malformed_duplicate_config_fails_closed(self):
        raw=encoded(self.config);self.path.write_bytes(raw.replace(b'"native-demo":"',b'"native-demo":"'+'b'.encode()*64+b'","native-demo":"',1))
        self.assertEqual(self.discovery()[0],403);self.assertEqual(sum(self.calls.values()),0)

if __name__=='__main__':unittest.main()
