import "./index.css";
import { Composition } from "remotion";
import { FPS, TOTAL, TuskLaunch } from "./TuskLaunch";

export const RemotionRoot: React.FC = () => (
  <Composition
    id="TuskLaunch"
    component={TuskLaunch}
    width={1920}
    height={1080}
    fps={FPS}
    durationInFrames={TOTAL}
    defaultProps={{ music: null as string | null }}
  />
);
