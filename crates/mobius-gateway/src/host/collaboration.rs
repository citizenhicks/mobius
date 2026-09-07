use super::*;
use crate::bots::swarm::validate_swarm_members;

impl GatewayHost {
    pub(crate) async fn create_swarm(
        &self,
        title: String,
        leader_bot_id: String,
        member_bot_ids: Vec<String>,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (swarm, ids) = {
            let state = self.state.lock().await;
            let mut ids = vec![leader_bot_id.clone()];
            ids.extend(
                member_bot_ids
                    .into_iter()
                    .filter(|bot_id| bot_id != &leader_bot_id),
            );
            for bot_id in &ids {
                state.bots.bot(bot_id).map_err(invalid_bot)?;
            }
            validate_swarm_members(&leader_bot_id, &ids).map_err(invalid_swarm)?;
            (Arc::clone(&state.swarm), ids)
        };
        swarm
            .create(title, leader_bot_id, ids)
            .await
            .map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn add_swarm_member(
        &self,
        swarm_id: &str,
        bot_id: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = {
            let state = self.state.lock().await;
            state.bots.bot(&bot_id).map_err(invalid_bot)?;
            Arc::clone(&state.swarm)
        };
        swarm.join(swarm_id, bot_id).await.map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn leave_swarm(
        &self,
        swarm_id: &str,
        bot_id: &str,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = {
            let state = self.state.lock().await;
            state.bots.bot(bot_id).map_err(invalid_bot)?;
            Arc::clone(&state.swarm)
        };
        swarm.leave(swarm_id, bot_id).await.map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn rename_swarm(
        &self,
        swarm_id: &str,
        title: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarms = {
            let state = self.state.lock().await;
            state
                .swarm
                .rename(swarm_id, title)
                .await
                .map_err(invalid_swarm)?;
            state.swarm.records().await.map_err(internal)?
        };
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn disband_swarm(
        &self,
        swarm_id: &str,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let (swarm, scratchpad) = {
            let state = self.state.lock().await;
            (Arc::clone(&state.swarm), state.scratchpad.clone())
        };
        disband_swarm_with_scratchpad(&swarm, &scratchpad, swarm_id).await?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }

    pub(crate) async fn post_swarm_message(
        &self,
        swarm_id: &str,
        text: String,
    ) -> std::result::Result<Vec<SwarmRecord>, Rejection> {
        let _mutation = self.begin_mutation().await?;
        let swarm = Arc::clone(&self.state.lock().await.swarm);
        swarm
            .post_user(swarm_id, text)
            .await
            .map_err(invalid_swarm)?;
        let swarms = swarm.records().await.map_err(internal)?;
        self.broadcast_swarms(&swarms);
        Ok(swarms)
    }
}

async fn disband_swarm_with_scratchpad(
    swarm: &SwarmStore,
    scratchpad: &ScratchpadStore,
    swarm_id: &str,
) -> std::result::Result<(), Rejection> {
    swarm.disband(swarm_id).await.map_err(invalid_swarm)?;
    let _ = scratchpad.clear_swarm(swarm_id).await;
    Ok(())
}
