use serenity::{
    async_trait,
    client::{Context, EventHandler},
    framework::{
        standard::{
            macros::{command, group},
            CommandResult,
        },
        StandardFramework,
    },
    model::{
        channel::Message,
        id::{ChannelId, GuildId},
        prelude::{Ready, VoiceState},
    },
    prelude::{Mutex, TypeMapKey},
    Client,
};
use songbird::{
    input::{self, cached::Memory},
    Call, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit, TrackEvent,
};
use std::{collections::HashMap, env, sync::Arc};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let token = env::var("DISCORD_TOKEN").expect("discord token");

    let framework = StandardFramework::new().group(&GENERAL_GROUP);

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .type_map_insert::<SoundStore>(HashMap::new())
        .register_songbird()
        .await
        .expect("successful client creation");

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("regiter ctrlc handler");
        info!("Shutting down");

        shard_manager.lock().await.shutdown_all().await;
    });

    if let Err(e) = client.start().await {
        error!("Client error: {e}");
    }

    info!("Bye!");
}

struct SoundStore;

impl TypeMapKey for SoundStore {
    type Value = HashMap<GuildId, Memory>;
}

const TOO_MUCH_ATTACH_MSG: &str =
    "Vou usar só o primeiro arquivo que tu mandou, o resto eu to ignorando!!";

#[group]
#[commands(set)]
struct General;

#[command]
async fn set(ctx: &Context, msg: &Message) -> CommandResult {
    let gid = if let Some(gid) = msg.guild_id {
        gid
    } else {
        return Ok(());
    };

    if msg.attachments.is_empty() {
        if let Err(e) = msg.reply(ctx, "Cadê o áudio carai??").await {
            warn!("Error replying: {e}");
        }
        return Ok(());
    }

    if msg.attachments.len() > 1 {
        if let Err(e) = msg.reply(ctx, TOO_MUCH_ATTACH_MSG).await {
            warn!("Error replying: {e}");
        }
    }

    match save_audio(ctx, msg, gid).await {
        Ok(()) => {
            if let Err(e) = msg.reply(ctx, "Blz, vou tocar esse áudio aí!!").await {
                warn!("Error replying: {e}");
            }
        }
        Err(_) => {
            if let Err(e) = msg.reply(ctx, "Deu pau").await {
                warn!("Error replying: {e}");
            }
        }
    }

    Ok(())
}

async fn save_audio(ctx: &Context, msg: &Message, gid: GuildId) -> Result<(), AudioError> {
    let attach = msg.attachments.first().expect("already checked size");
    match attach.download().await {
        Ok(content) => {
            let track = track_from(&content, gid, &attach.filename).await?;

            let mut data = ctx.data.write().await;
            let sound_store = data.get_mut::<SoundStore>().expect("sound store is set");
            sound_store.insert(gid, track);

            Ok(())
        }
        Err(e) => {
            warn!("Error downloading user audio: {}", e);
            Ok(())
        }
    }
}

use songbird::input::error::Error as AudioError;

async fn track_from(content: &[u8], gid: GuildId, name: &str) -> Result<Memory, AudioError> {
    let path = env::temp_dir().join(format!("{}{}", gid, name));
    {
        match File::create(&path).await {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content).await {
                    warn!("Error writing file: {e}");
                }
            }
            Err(e) => warn!("Error creating file: {e}"),
        }
    }

    let track = {
        let track_input = input::ffmpeg(&path).await?;
        Memory::new(track_input)?
    };

    // if let Err(e) = remove_file(path).await {
    //     warn!("Error deleting file: {e}");
    // }

    Ok(track)
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Connected as {}", ready.user.name);
    }

    async fn voice_state_update(
        &self,
        ctx: Context,
        gid: Option<GuildId>,
        old: Option<VoiceState>,
        new: VoiceState,
    ) {
        if is_bot(&new) {
            return;
        }

        let gid = if let Some(gid) = gid {
            gid
        } else {
            return;
        };

        // if somebody joined some channel
        if let Some(channel_id) = joined_channel(old.as_ref(), &new) {
            let data = ctx.data.read().await;
            let sound_store = data.get::<SoundStore>().expect("sound store is set");

            // if there is a sound set to play on the guild
            if let Some(sound) = sound_store.get(&gid) {
                let manager = songbird::get(&ctx).await.expect("songbird is set");

                // TODO: check if not already playing on another channel

                // join channel
                let (call, res) = manager.join(gid, channel_id).await;
                if let Err(e) = res {
                    warn!("Error joining channel: {e}");
                } else {
                    let handle = {
                        let input = sound
                            .new_handle()
                            .try_into()
                            .expect("created from an input, converting back should work");

                        call.lock().await.play_only_source(input)
                    };
                    handle
                        .add_event(Event::Track(TrackEvent::End), Disconnect { call })
                        .expect("do not return error for valid events");
                }
            }
        }
    }
}

fn is_bot(vs: &VoiceState) -> bool {
    if let Some(memb) = &vs.member {
        memb.user.bot
    } else {
        false
    }
}

fn joined_channel(old: Option<&VoiceState>, new: &VoiceState) -> Option<ChannelId> {
    if old.and_then(|old| old.channel_id) != new.channel_id {
        new.channel_id
    } else {
        None
    }
}

struct Disconnect {
    call: Arc<Mutex<Call>>,
}

#[async_trait]
impl VoiceEventHandler for Disconnect {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(_) = ctx {
            if let Err(e) = self.call.lock().await.leave().await {
                warn!("Error leaving channel: {e}");
            }
        }
        None
    }
}
