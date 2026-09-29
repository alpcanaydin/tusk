import "./index.css";
import { Composition } from "remotion";
import { FPS, TOTAL, TuskLaunch } from "./TuskLaunch";
import { REDDIT_FPS, REDDIT_FRAMES, RedditDemo } from "./RedditDemo";

export const RemotionRoot: React.FC = () => (
  <>
    <Composition
      id="TuskLaunch"
      component={TuskLaunch}
      width={1920}
      height={1080}
      fps={FPS}
      durationInFrames={TOTAL}
      defaultProps={{ music: null as string | null }}
    />
    <Composition
      id="RedditDemo"
      component={RedditDemo}
      width={1920}
      height={1080}
      fps={REDDIT_FPS}
      durationInFrames={REDDIT_FRAMES}
    />
  </>
);
