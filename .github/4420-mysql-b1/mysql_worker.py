#!/usr/bin/env python3.9
"""Native PyMySQL branch cases only. Invoked by the root-gated controller."""
import argparse
import concurrent.futures
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import sys
import threading
import time


RESET_CONNECTION = 0x1F
GONE_MARKER = 'USE BRANCH main or another branch'


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--execute',action='store_true')
    parser.add_argument('--port',type=int,required=True)
    parser.add_argument('--server-identity',type=Path,required=True)
    parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--client-package',type=Path,required=True)
    args=parser.parse_args()
    if not args.execute or os.environ.get('NANO_PERF_BOUNDED_SCOPE')!='24G-noswap':
        raise SystemExit('worker requires explicit root-controller execution')
    sys.path.insert(0,str(args.client_package.resolve().parent))
    import pymysql
    from pymysql.constants import SERVER_STATUS
    assert Path(pymysql.__file__).resolve()==args.client_package.resolve()/'__init__.py','wrong client package loaded'
    identity=json.loads(args.server_identity.read_text())
    command=identity['command']
    assert command[command.index('--mysql-listen')+1]=='127.0.0.1:'+str(args.port),'worker endpoint differs from owned daemon'
    assert '--memory' in command and command[command.index('--http-port')+1]=='0','worker requires isolated memory/http-disabled daemon'
    def owned():
        try:
            pid=identity['pid']
            ticks=int(Path('/proc/%s/stat'%pid).read_text().rsplit(')',1)[1].split()[19])
            argv=Path('/proc/%s/cmdline'%pid).read_bytes().split(b'\0')
            if argv and argv[-1]==b'':argv.pop()
            return ticks==identity['start_ticks'] and argv==[os.fsencode(arg) for arg in identity['command']] and Path('/proc/%s/exe'%pid).resolve()==Path(identity['command'][0]).resolve()
        except (OSError,ValueError,KeyError):return False
    events=[];lock=threading.Lock();connections={};phases=[];errors=[]
    def event(value):
        with lock:
            value['sequence']=len(events);events.append(value)
    def execute(actor,sql,expected=None,affected=None):
        assert owned(),'owned daemon identity changed'
        connection=connections[actor]
        started=time.monotonic_ns()
        try:
            with connection.cursor() as cursor:
                count=cursor.execute(sql)
                rows=[list(row) for row in cursor.fetchall()] if cursor.description else []
            record={'actor':actor,'sql':sql,'rows':rows,'affected_rows':count,'elapsed_ns':time.monotonic_ns()-started,'error':None}
            event(record)
            if expected is not None:assert rows==expected,(actor,sql,rows,expected)
            if affected is not None:assert count==affected,(actor,sql,count,affected)
            return rows
        except pymysql.MySQLError as error:
            event({'actor':actor,'sql':sql,'error_code':error.args[0] if error.args else None,'error_message':str(error),'elapsed_ns':time.monotonic_ns()-started})
            raise
    def expect_gone(actor,sql):
        try:execute(actor,sql)
        except pymysql.MySQLError as error:
            assert error.args[0]==1105 and GONE_MARKER in str(error) and 'mysql_a' in str(error) and 'dropped' in str(error),str(error)
            return
        raise AssertionError('stale branch binding succeeded: '+sql)
    def reset(actor):
        assert owned(),'owned daemon identity changed'
        connection=connections[actor]
        connection._execute_command(RESET_CONNECTION,b'')
        ok=connection._read_ok_packet()
        event({'actor':actor,'command':'COM_RESET_CONNECTION','command_byte':RESET_CONNECTION,'server_status':ok.server_status})
        assert not(ok.server_status & SERVER_STATUS.SERVER_STATUS_IN_TRANS),'reset left transaction status set'
    def connect(actor):
        assert owned(),'owned daemon identity changed'
        connection=pymysql.connect(host='127.0.0.1',port=args.port,user='root',password='',database=None,
          charset='utf8mb4',autocommit=True,connect_timeout=3,read_timeout=8,write_timeout=8)
        connections[actor]=connection
        event({'actor':actor,'connection_id':connection.thread_id(),'connected':True})
    def parallel(name,operation,tx=False):
        start=threading.Barrier(3,timeout=10)
        opened=threading.Barrier(3,timeout=10) if tx else None
        def actor_run(actor):
            start.wait()
            if tx:
                execute(actor,'BEGIN')
                assert connections[actor].server_status & SERVER_STATUS.SERVER_STATUS_IN_TRANS,'BEGIN not in transaction'
                opened.wait()
            return operation(actor)
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
            futures=[executor.submit(actor_run,actor) for actor in ['main','a','b']]
            for future in futures:future.result(timeout=30)
        phases.append({'name':name,'connections':3,'overlapping_explicit_transactions':bool(tx),'completed':True})
    def rows(actor,expected):return execute(actor,'SELECT id,v FROM mysql_probe ORDER BY id',expected=expected)
    base={'main':[[1,'main_after_fork'],[2,'main_two'],[3,'main_only']],
          'a':[[1,'main_seed'],[2,'main_two']], 'b':[[1,'main_seed'],[2,'main_two']]}
    isolated={actor:data+[[10,actor+'_only10']] for actor,data in base.items()}
    committed={actor:[[1,actor+'_commit']]+data[1:]+[[20,actor+'_only20']] for actor,data in isolated.items()}
    try:
        for actor in ['main','a','b']:connect(actor)
        assert len({connection.thread_id() for connection in connections.values()})==3,'connections do not have distinct wire IDs'
        execute('main','CREATE TABLE mysql_probe(id INT PRIMARY KEY,v TEXT)')
        execute('main',"INSERT INTO mysql_probe VALUES(1,'main_seed'),(2,'main_two')",affected=2)
        execute('main','CREATE BRANCH mysql_a FROM main')
        execute('main','CREATE BRANCH mysql_b FROM main')
        execute('a','USE BRANCH mysql_a');execute('b','USE BRANCH mysql_b')
        execute('main',"UPDATE mysql_probe SET v='main_after_fork' WHERE id=1",affected=1)
        execute('main',"INSERT INTO mysql_probe VALUES(3,'main_only')",affected=1)
        parallel('immutable_forks',lambda actor:rows(actor,base[actor]))
        for actor,expected in [('main','main'),('a','mysql_a'),('b','mysql_b')]:execute(actor,'SHOW heliosdb.branch',expected=[[expected]])
        parallel('isolated_same_primary_key_writes',lambda actor:execute(actor,"INSERT INTO mysql_probe VALUES(10,'%s_only10')"%actor,affected=1))
        parallel('isolated_reads',lambda actor:rows(actor,isolated[actor]))
        def rollback(actor):
            execute(actor,"UPDATE mysql_probe SET v='%s_rolled_back' WHERE id=1"%actor,affected=1)
            execute(actor,"INSERT INTO mysql_probe VALUES(99,'%s_rollback_only')"%actor,affected=1)
            own=[[1,actor+'_rolled_back']]+isolated[actor][1:]+[[99,actor+'_rollback_only']]
            rows(actor,own);execute(actor,'ROLLBACK');rows(actor,isolated[actor])
        parallel('overlapping_transactions_rollback',rollback,tx=True)
        def commit(actor):
            execute(actor,"UPDATE mysql_probe SET v='%s_commit' WHERE id=1"%actor,affected=1)
            execute(actor,"INSERT INTO mysql_probe VALUES(20,'%s_only20')"%actor,affected=1)
            rows(actor,committed[actor]);execute(actor,'COMMIT');rows(actor,committed[actor])
        parallel('overlapping_transactions_commit',commit,tx=True)
        execute('main','DROP BRANCH mysql_a')
        execute('main','CREATE BRANCH mysql_a FROM main')
        # An old immutable binding must not silently pick up the re-created ID.
        expect_gone('a','SELECT id,v FROM mysql_probe ORDER BY id')
        expect_gone('a',"INSERT INTO mysql_probe VALUES(777,'stale_leak')")
        expect_gone('a','SELECT id,v FROM mysql_probe ORDER BY id')
        rows('main',committed['main']);rows('b',committed['b'])
        reset('a');execute('a','SHOW heliosdb.branch',expected=[['main']]);rows('a',committed['main'])
        execute('a','USE BRANCH mysql_a');rows('a',committed['main'])
        execute('a','USE BRANCH main')
        execute('b','BEGIN')
        assert connections['b'].server_status & SERVER_STATUS.SERVER_STATUS_IN_TRANS,'reset scenario BEGIN not in transaction'
        execute('b',"UPDATE mysql_probe SET v='b_reset_uncommitted' WHERE id=1",affected=1)
        execute('b',"INSERT INTO mysql_probe VALUES(77,'b_reset_leak')",affected=1)
        rows('b',[[1,'b_reset_uncommitted']]+committed['b'][1:]+[[77,'b_reset_leak']])
        reset('b');execute('b','SHOW heliosdb.branch',expected=[['main']]);rows('b',committed['main'])
        execute('a','USE BRANCH mysql_b');rows('a',committed['b'])
        execute('b',"INSERT INTO mysql_probe VALUES(30,'main_after_reset')",affected=1)
        final_main=committed['main']+[[30,'main_after_reset']]
        rows('main',final_main);rows('b',final_main);rows('a',committed['b'])
        phases.append({'name':'dropped_id_refusal_and_protocol_reset','completed':True})
    except BaseException as error:
        errors.append(type(error).__name__+': '+str(error))
    finally:
        for actor,connection in connections.items():
            try:connection.close()
            except BaseException as error:errors.append('close '+actor+': '+str(error))
        receipt={'schema_version':1,'evidence_kind':'native_mysql_b1_worker','complete':not errors and len(phases)==6,
          'errors':errors,'phases':phases,'events':events,'python_executable':sys.executable,'python_version':sys.version,
          'pymysql_file':pymysql.__file__,'pymysql_distribution_version':importlib.metadata.version('PyMySQL'),
          'pymysql_VERSION_STRING':pymysql.VERSION_STRING,'pymysql_compatibility___version__':pymysql.__version__,
          'release_acceptance_pass':False}
        with args.out.open('x') as stream:json.dump(receipt,stream,indent=2);stream.write('\n')
        print(json.dumps({'complete':receipt['complete'],'phases':len(phases),'errors':errors,'release_acceptance_pass':False}))
    return 0 if receipt['complete'] else 1


if __name__=='__main__':sys.exit(main())
