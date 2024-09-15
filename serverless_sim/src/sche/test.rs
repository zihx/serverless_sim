use std::{
    borrow::Borrow,
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use daggy::Walker;
use rand::Rng;

use crate::{
    fn_dag::{DagId, EnvFnExt, FnId},
    mechanism::{MechanismImpl, ScheCmd, SimEnvObserve},
    mechanism_thread::{MechCmdDistributor, MechScheduleOnceRes},
    node::{EnvNodeExt, NodeId},
    request::Request,
    sim_run::Scheduler,
    with_env_sub::{WithEnvCore, WithEnvHelp},
};

pub struct TestScheduler {
    // DAG中每个函数的优先级
    dag_fns_priority: HashMap<DagId, Vec<(FnId, f32)>>,

    // 每个节点剩余的cpu
    node_resource_left: HashMap<NodeId, (f32, f32)>,

    // 每一对节点之间的bandwidth
    node2node_all_bw: Vec<f32>,

    // 每个节点上的任务总数（running + pending）、running任务数
    node_task_count: HashMap<NodeId, (usize, usize)>,

    // 单位内存的开销（不固定）
    mem_cost_per_unit: f32,
    mode: String,
}

impl TestScheduler {
    pub fn new(arg: &str) -> Self {
        let n = arg.len();
        let a = arg.get(0..n - 1).unwrap();
        let b = arg.get(n - 1..n).unwrap();
        Self {
            dag_fns_priority: HashMap::new(),
            node_resource_left: HashMap::new(),
            node2node_all_bw: Vec::new(),
            node_task_count: HashMap::new(),
            mem_cost_per_unit: a.parse::<f32>().unwrap(),
            mode: b.to_string(),
        }
    }

    // 初始化node_cpu_left
    fn prepare(&mut self, env: &SimEnvObserve) {
        for node in env.nodes().iter() {
            self.node_resource_left.insert(
                node.node_id(),
                (
                    node.rsc_limit.cpu - node.last_frame_cpu,
                    node.rsc_limit.mem - node.last_frame_mem,
                ),
            );

            self.node_task_count.insert(
                node.node_id(),
                (node.all_task_cnt(), node.running_task_cnt()),
            );
        }
    }

    // 初始化node2node_all_bw
    fn prepare_node2node_all_bw(&mut self, env: &SimEnvObserve) {
        let node_count = env.nodes().len();
        for i in 0..node_count {
            for j in 0..i {
                self.node2node_all_bw
                    .push(env.core().node2node_graph()[i][j]);
            }
        }
    }

    // 初始化dag_fns_priority
    fn calculate_priority_for_dag_fns(&mut self, req: &Request, env: &SimEnvObserve) {
        let dag = env.dag(req.dag_i);

        if !self.dag_fns_priority.contains_key(&dag.dag_i) {
            let mut map: HashMap<usize, f32> = HashMap::new();
            let mut walker = dag.new_dag_walker();
            let mut stack = vec![];

            // 拓扑排序
            while let Some(func_g_i) = walker.next(&dag.dag_inner) {
                let fnid = dag.dag_inner[func_g_i];
                let func = env.func(fnid);

                let mut t_sum_exec = 0.0;
                for node in env.nodes().iter() {
                    let node_cpu_left = self.node_resource_left.get(&node.node_id()).unwrap().0;
                    // let node_running_task_count =
                    //     self.node_task_count.get(&node.node_id()).unwrap().1;

                    // let each_running_task_cpu = node_cpu_left / node_running_task_count as f32;

                    // t_sum_exec += func.cpu / each_running_task_cpu;
                    t_sum_exec += func.cpu / node_cpu_left;
                }
                // 函数平均执行时间
                let t_avg_exec = t_sum_exec / self.node_resource_left.len() as f32;

                let mut t_sum_trans = 0.0;
                for bw in &self.node2node_all_bw {
                    t_sum_trans += func.out_put_size / bw * 5.0;
                }
                // 平均数据传输时间
                let t_avg_trans = t_sum_trans / self.node2node_all_bw.len() as f32;

                // 函数内存占用
                let t_mem_cost = func.mem as f32 * self.mem_cost_per_unit;

                log::info!(
                    "t_avg_exec{} t_avg_trans{} t_mem_cost{}",
                    t_avg_exec,
                    t_avg_trans,
                    t_mem_cost
                );

                // 总开销，用于后续定义优先级
                let total_cost = t_avg_exec + t_avg_trans - t_mem_cost;

                map.insert(fnid, total_cost);

                // 利用栈LIFO的特性，记录逆拓扑
                stack.push(func_g_i);
            }

            // 按拓扑排序的逆序为每一个函数计算priority，因为函数的优先级与其后继有关
            while let Some(func_g_i) = stack.pop() {
                let nexts: daggy::Children<usize, f32, u32> = dag.dag_inner.children(func_g_i);
                // 取后继中优先级最大的
                if let Some(max_node) = nexts.iter(&dag.dag_inner).max_by(|a, b| {
                    let fnid_a = dag.dag_inner[a.1];
                    let fnid_b = dag.dag_inner[b.1];

                    map.get(&fnid_a)
                        .unwrap()
                        .total_cmp(map.get(&fnid_b).unwrap())
                }) {
                    let fnid_max = dag.dag_inner[max_node.1];
                    let max = *map.get(&fnid_max).unwrap();

                    let fnid = dag.dag_inner[func_g_i];
                    (*map.get_mut(&fnid).unwrap()) += max;
                }
            }

            let mut prio_order = map.into_iter().collect::<Vec<_>>();

            // 降序排序，优先调度优先级高的函数
            prio_order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or_else(|| Ordering::Equal));

            // 记录当前dag中函数的优先级序列,避免重复计算
            self.dag_fns_priority.insert(dag.dag_i, prio_order);
        }
    }

    fn schedule_one_req(
        &mut self,
        env: &SimEnvObserve,
        mech: &MechanismImpl,
        req: &Request,
        cmd_distributor: &MechCmdDistributor,
    ) {
        // 获取该请求中可以被调度的函数（即前驱已被调度的函数）以及含有该函数的容器的节点
        let mut scheduleable_fns_nodes = HashMap::new();
        let dag = env.dag(req.dag_i);
        let mut walker = dag.new_dag_walker();
        'next_fn: while let Some(func_g_i) = walker.next(&dag.dag_inner) {
            let fnid = dag.dag_inner[func_g_i];

            // 函数已被调度，跳过
            if req.fn_node.contains_key(&fnid) {
                continue;
            }

            // 函数的前驱尚未被调度，跳过
            let predecessors = env.func(fnid).parent_fns(env);
            for p in &predecessors {
                if !req.fn_node.contains_key(p) {
                    continue 'next_fn;
                }
            }

            if scheduleable_fns_nodes.contains_key(&fnid) {
                continue;
            }

            scheduleable_fns_nodes.insert(
                fnid,
                env.core()
                    .fn_2_nodes()
                    .get(&fnid)
                    .map(|v| v.clone())
                    .unwrap_or(HashSet::new()),
            );
        }

        let mech_metric = || env.help().mech_metric_mut();

        let mut scheduleable_fns = Vec::new();
        self.calculate_priority_for_dag_fns(req, env);
        let dag_fns_priority = self.dag_fns_priority.get(&req.dag_i).unwrap();
        // 根据优先级排序可调度的函数
        for (fn_id, _) in dag_fns_priority {
            if scheduleable_fns_nodes.contains_key(fn_id) {
                scheduleable_fns.push(fn_id);
            }
        }

        for &fnid in scheduleable_fns {
            // scale_sche_joint在调度前已经更新了函数所需容器的数量，获取
            let mut target_cnt = mech.scale_num(fnid);
            if target_cnt == 0 {
                target_cnt = 1;
            }

            // 扩容
            let fn_scale_up_cmds =
                mech.scale_up_exec()
                    .exec_scale_up(target_cnt, fnid, env, cmd_distributor);

            // 将扩容所选的节点加入函数的可调度节点
            // 函数的可调度节点 = 已经有容器的节点 + 扩容所选的节点
            for cmd in fn_scale_up_cmds.iter() {
                scheduleable_fns_nodes
                    .get_mut(&cmd.fnid)
                    .unwrap()
                    .insert(cmd.nid);
            }

            let scheduleable_nodes = scheduleable_fns_nodes.get(&fnid).unwrap();
            let mut best_score = 0.0;
            let mut best_node_id = 99;
            for node_id in scheduleable_nodes {
                // 函数的前驱列表
                let predecessors = env.func(fnid).parent_fns(env);
                let mut not_in_the_same_node = 0;
                let mut transimission_time = 0.0;
                for pred in predecessors {
                    let pred_node_id = req.fn_node.get(&pred).unwrap();
                    // 前驱没有调度到当前节点
                    if pred_node_id != node_id {
                        not_in_the_same_node += 1;
                        transimission_time += env.func(fnid).out_put_size
                            / env.node_get_speed_btwn(*pred_node_id, *node_id);
                    }
                }

                let node_task_count = self.node_task_count.get(node_id).unwrap().0;
                let node_running_task_count = self.node_task_count.get(node_id).unwrap().1;
                let node_cpu_left = self.node_resource_left.get(node_id).unwrap().0;
                let node_mem_left = self.node_resource_left.get(node_id).unwrap().1;

                let each_running_task_cpu = node_cpu_left / node_running_task_count as f32;

                // 优先调度到任务总数少, 无需数据传输(即与前驱部署到同一节点)
                let mut score_this_node = 0.0;
                if self.mode == "a" {
                    score_this_node = 1.0 / (node_task_count as f32 + 1.0)
                    // + 1.0 / (not_in_the_same_node as f32 + 1.0)
                    - transimission_time
                    // + env.func(fnid).cpu / each_running_task_cpu as f32
                    + env.func(fnid).cpu / node_cpu_left as f32
                    + env.func(fnid).mem / node_mem_left as f32;
                } else if self.mode == "b" {
                    score_this_node = 1.0 / (node_task_count as f32 + 1.0)
                    + 1.0 / (not_in_the_same_node as f32 + 1.0)
                    // - transimission_time
                    // + env.func(fnid).cpu / each_running_task_cpu as f32
                    + env.func(fnid).cpu / node_cpu_left as f32
                    + env.func(fnid).mem / node_mem_left as f32;
                } else {
                    score_this_node = 1.0 / (node_task_count as f32 + 1.0)
                    + 1.0 / (not_in_the_same_node as f32 + 1.0)
                    - transimission_time
                    // + env.func(fnid).cpu / each_running_task_cpu as f32
                    + env.func(fnid).cpu / node_cpu_left as f32
                    + env.func(fnid).mem / node_mem_left as f32;
                }
                // + node_mem_left / env.node(*node_id).rsc_limit.mem;

                // log::info!("score_this_node {}", score_this_node);
                // log::info!("best_score {}", best_score);

                if score_this_node > best_score {
                    best_score = score_this_node;
                    best_node_id = *node_id;
                }
            }

            if best_node_id == 99 {
                best_node_id = rand::thread_rng().gen_range(0..env.nodes().len());
            }

            mech_metric().add_node_task_new_cnt(best_node_id);

            log::info!("best_node_id {}", best_node_id);

            // best_node任务总数 + 1
            if let Some((all_task_count, _)) = self.node_task_count.get_mut(&best_node_id) {
                *all_task_count += 1;
            }

            // 调度指令
            cmd_distributor
                .send(MechScheduleOnceRes::ScheCmd(ScheCmd {
                    reqid: req.req_id,
                    fnid,
                    nid: best_node_id,
                    memlimit: None,
                }))
                .unwrap();
        }
    }
}

impl Scheduler for TestScheduler {
    fn schedule_some(
        &mut self,
        env: &SimEnvObserve,
        mech: &MechanismImpl,
        cmd_distributor: &MechCmdDistributor,
    ) {
        // 清理上一次调度的数据
        self.node_resource_left.clear();
        self.node2node_all_bw.clear();
        self.node_task_count.clear();

        self.prepare(env);

        // 获取每一对节点的bandwidth
        self.prepare_node2node_all_bw(env);

        for (_req_id, req) in env.core().requests().iter() {
            self.schedule_one_req(env, mech, req, cmd_distributor);
        }

        // 缩容
        for func in env.core().fns().iter() {
            let target = mech.scale_num(func.fn_id);
            let cur = env.fn_container_cnt(func.fn_id);
            if target < cur {
                mech.scale_down_exec().exec_scale_down(
                    env,
                    func.fn_id,
                    cur - target,
                    cmd_distributor,
                );
            }
        }
    }
}
